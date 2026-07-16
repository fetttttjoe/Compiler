//! Monomorphization (ADR 0035). Templates are parsed but never checked;
//! each distinct type-argument list clones the AST with parameters
//! substituted (`TypeAnn::Resolved` carries module identity) and spans
//! shifted into a fresh `SourceMap` range, then checks the clone like
//! any function. Both engines only ever see the monomorphic expansion.

use std::collections::{HashMap, HashSet};

use crate::ast::{EnumDecl, Expr, Function, MatchArm, Param, Stmt, Struct, TypeAnn};
use crate::span::Span;
use crate::types::{EnumType, StructType, Type, instance_name};

/// Instantiation-chain ceiling: `f<T>` requesting `f<T[]>` (or a struct
/// family doing the same through a field) diagnoses instead of
/// expanding forever.
pub(super) const DEPTH_CAP: u32 = 32;

/// An item's identity: (defining module, name).
pub(super) type ItemKey = (usize, String);

/// Structural view of struct/enum instances: instance key → (template
/// key, type arguments). Inference decomposes instantiated types
/// through it — the type namespaces are shared, so keys never collide.
pub(super) type InstanceArgs = HashMap<ItemKey, (ItemKey, Vec<Type>)>;

/// One pending function instantiation.
pub(super) struct FnWork {
    pub template: (usize, String),
    pub args: Vec<Type>,
    /// Instantiation-chain depth of the requesting body plus one.
    pub depth: u32,
}

/// Grow-only generic state shared by every checking phase. Owns the
/// struct-layout table so instantiation can insert mid-check; the
/// checker reads layouts through it.
pub(super) struct Mono<'g> {
    pub fn_templates: HashMap<ItemKey, &'g Function>,
    pub struct_templates: HashMap<ItemKey, &'g Struct>,
    pub enum_templates: HashMap<ItemKey, &'g EnumDecl>,
    /// Every struct layout — monomorphic declarations and instances
    /// alike. Moves into `Resolutions` when checking completes.
    pub structs: HashMap<ItemKey, StructType>,
    /// Every enum definition, same story (ADR 0036).
    pub enums: HashMap<ItemKey, EnumType>,
    pub instance_args: InstanceArgs,
    pub work: Vec<FnWork>,
    /// Function instances already requested (dedup across call sites).
    pub requested: HashSet<ItemKey>,
    /// Struct/enum-instantiation nesting — the family-divergence fuel.
    pub struct_depth: u32,
}

impl Mono<'_> {
    pub fn new() -> Mono<'static> {
        Mono {
            fn_templates: HashMap::new(),
            struct_templates: HashMap::new(),
            enum_templates: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            instance_args: HashMap::new(),
            work: Vec::new(),
            requested: HashSet::new(),
            struct_depth: 0,
        }
    }
}

/// Collects type-parameter bindings by walking a template annotation
/// against an actual argument type, unwrapping along exactly the edges
/// `fits` lets values flow. Never validates — mismatches surface later
/// as ordinary argument-type errors. `null`/`[]`/poison bind nothing.
/// `Err` is a conflict: (parameter, first binding, second binding).
pub(super) fn unify(
    ann: &TypeAnn,
    actual: &Type,
    tparams: &HashSet<&str>,
    bind: &mut HashMap<String, Type>,
    instance_args: &InstanceArgs,
    ty_alias: &super::Alias,
) -> Result<(), (String, Type, Type)> {
    match ann {
        TypeAnn::Named(n) if tparams.contains(n.as_str()) => {
            // No-information values: `null`, `[]` shapes, unit calls,
            // and poisoned recovery all leave the parameter open.
            if matches!(actual, Type::Null | Type::Unit | Type::Error)
                || crate::types::unconstrained(actual)
            {
                return Ok(());
            }
            match bind.get(n) {
                Some(prev) if prev != actual => Err((n.clone(), prev.clone(), actual.clone())),
                Some(_) => Ok(()),
                None => {
                    bind.insert(n.clone(), actual.clone());
                    Ok(())
                }
            }
        }
        // `T?` accepts `X?` or a bare `X` (the fits subsumption edge).
        TypeAnn::Optional(inner) => match actual {
            Type::Optional(a) => unify(inner, a, tparams, bind, instance_args, ty_alias),
            other => unify(inner, other, tparams, bind, instance_args, ty_alias),
        },
        // Same edge for `T!`: values flow into unions.
        TypeAnn::ErrUnion(inner) => match actual {
            Type::ErrUnion(a) => unify(inner, a, tparams, bind, instance_args, ty_alias),
            other => unify(inner, other, tparams, bind, instance_args, ty_alias),
        },
        // Arrays are invariant: only an array argument informs `T[]`.
        TypeAnn::Array(inner) => match actual {
            Type::Array(a) => unify(inner, a, tparams, bind, instance_args, ty_alias),
            _ => Ok(()),
        },
        // `Pair<T, U>` against an instantiated struct or enum:
        // decompose when the argument instantiates the same template.
        TypeAnn::Applied(n, anns) => {
            let (am, an) = match actual {
                Type::Struct(am, an) | Type::Enum(am, an) => (am, an),
                _ => return Ok(()),
            };
            let Some(key) = ty_alias.get(n) else {
                return Ok(());
            };
            let Some((tkey, args)) = instance_args.get(&(*am, an.clone())) else {
                return Ok(());
            };
            if tkey != key || anns.len() != args.len() {
                return Ok(());
            }
            for (ann, arg) in anns.iter().zip(args) {
                unify(ann, arg, tparams, bind, instance_args, ty_alias)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Rewrites a template annotation with its type parameters bound —
/// bound names become `Resolved` types, so module identity survives
/// into the clone. An applied type param (`T<int>`) stays unresolved
/// and reports as an unknown type at instance-check time.
pub(super) fn substitute_ann(ann: &TypeAnn, bind: &HashMap<String, Type>) -> TypeAnn {
    match ann {
        TypeAnn::Named(n) => match bind.get(n) {
            Some(t) => TypeAnn::Resolved(t.clone()),
            None => TypeAnn::Named(n.clone()),
        },
        TypeAnn::Applied(n, args) => TypeAnn::Applied(
            n.clone(),
            args.iter().map(|a| substitute_ann(a, bind)).collect(),
        ),
        TypeAnn::Optional(inner) => TypeAnn::Optional(Box::new(substitute_ann(inner, bind))),
        TypeAnn::Array(inner) => TypeAnn::Array(Box::new(substitute_ann(inner, bind))),
        TypeAnn::ErrUnion(inner) => TypeAnn::ErrUnion(Box::new(substitute_ann(inner, bind))),
        other => other.clone(),
    }
}

/// The instance key for a function template applied to `args`.
pub(super) fn fn_instance_key(template: &(usize, String), args: &[Type]) -> (usize, String) {
    (template.0, instance_name(&template.1, args))
}

/// Clones a template function with type parameters substituted and
/// every span shifted by `delta` — the respan that keeps the span-keyed
/// tables collision-free while diagnostics and traps still resolve to
/// the original source line. The clone is monomorphic: its name is the
/// canonical instance name and it carries no type parameters.
pub(super) fn instantiate_fn(
    f: &Function,
    name: String,
    bind: &HashMap<String, Type>,
    delta: usize,
) -> Function {
    Function {
        exported: f.exported,
        name,
        type_params: Vec::new(),
        params: f
            .params
            .iter()
            .map(|p| Param {
                name: p.name.clone(),
                ty: substitute_ann(&p.ty, bind),
            })
            .collect(),
        return_type: f.return_type.as_ref().map(|t| substitute_ann(t, bind)),
        body: f.body.iter().map(|s| clone_stmt(s, bind, delta)).collect(),
        span: shift(f.span, delta),
    }
}

fn shift(span: Span, delta: usize) -> Span {
    Span::new(span.start + delta, span.end + delta)
}

fn clone_stmt(stmt: &Stmt, bind: &HashMap<String, Type>, delta: usize) -> Stmt {
    match stmt {
        Stmt::Let {
            mutable,
            name,
            ty,
            value,
            span,
        } => Stmt::Let {
            mutable: *mutable,
            name: name.clone(),
            ty: ty.as_ref().map(|t| substitute_ann(t, bind)),
            value: clone_expr(value, bind, delta),
            span: shift(*span, delta),
        },
        Stmt::Assign {
            target,
            value,
            span,
        } => Stmt::Assign {
            target: clone_expr(target, bind, delta),
            value: clone_expr(value, bind, delta),
            span: shift(*span, delta),
        },
        Stmt::Return { value, span } => Stmt::Return {
            value: value.as_ref().map(|v| clone_expr(v, bind, delta)),
            span: shift(*span, delta),
        },
        Stmt::Break { span } => Stmt::Break {
            span: shift(*span, delta),
        },
        Stmt::Continue { span } => Stmt::Continue {
            span: shift(*span, delta),
        },
        Stmt::If {
            cond,
            then_body,
            else_body,
            span,
        } => Stmt::If {
            cond: clone_expr(cond, bind, delta),
            then_body: then_body
                .iter()
                .map(|s| clone_stmt(s, bind, delta))
                .collect(),
            else_body: else_body
                .as_ref()
                .map(|b| b.iter().map(|s| clone_stmt(s, bind, delta)).collect()),
            span: shift(*span, delta),
        },
        Stmt::While { cond, body, span } => Stmt::While {
            cond: clone_expr(cond, bind, delta),
            body: body.iter().map(|s| clone_stmt(s, bind, delta)).collect(),
            span: shift(*span, delta),
        },
        Stmt::For {
            index,
            name,
            iterable,
            body,
            span,
        } => Stmt::For {
            index: index.clone(),
            name: name.clone(),
            iterable: clone_expr(iterable, bind, delta),
            body: body.iter().map(|s| clone_stmt(s, bind, delta)).collect(),
            span: shift(*span, delta),
        },
        Stmt::Match {
            scrutinee,
            arms,
            else_body,
            span,
        } => Stmt::Match {
            scrutinee: clone_expr(scrutinee, bind, delta),
            arms: arms
                .iter()
                .map(|a| MatchArm {
                    variant: a.variant.clone(),
                    variant_span: shift(a.variant_span, delta),
                    bindings: a
                        .bindings
                        .iter()
                        .map(|(n, s)| (n.clone(), shift(*s, delta)))
                        .collect(),
                    body: a.body.iter().map(|s| clone_stmt(s, bind, delta)).collect(),
                    span: shift(a.span, delta),
                })
                .collect(),
            else_body: else_body
                .as_ref()
                .map(|b| b.iter().map(|s| clone_stmt(s, bind, delta)).collect()),
            span: shift(*span, delta),
        },
        Stmt::Expr(e) => Stmt::Expr(clone_expr(e, bind, delta)),
    }
}

fn clone_expr(expr: &Expr, bind: &HashMap<String, Type>, delta: usize) -> Expr {
    let sub = |e: &Expr| Box::new(clone_expr(e, bind, delta));
    match expr {
        Expr::Int(n, s) => Expr::Int(*n, shift(*s, delta)),
        Expr::Float(f, s) => Expr::Float(*f, shift(*s, delta)),
        Expr::Bool(b, s) => Expr::Bool(*b, shift(*s, delta)),
        Expr::Str(t, s) => Expr::Str(t.clone(), shift(*s, delta)),
        Expr::Ident(n, s) => Expr::Ident(n.clone(), shift(*s, delta)),
        Expr::Null(s) => Expr::Null(shift(*s, delta)),
        Expr::ErrorLit(n, s) => Expr::ErrorLit(n.clone(), shift(*s, delta)),
        Expr::ErrorKind(s) => Expr::ErrorKind(shift(*s, delta)),
        Expr::Try { expr, span } => Expr::Try {
            expr: sub(expr),
            span: shift(*span, delta),
        },
        Expr::Unary { op, rhs, span } => Expr::Unary {
            op: *op,
            rhs: sub(rhs),
            span: shift(*span, delta),
        },
        Expr::Convert {
            to,
            implicit,
            arg,
            span,
        } => Expr::Convert {
            to: *to,
            implicit: *implicit,
            arg: sub(arg),
            span: shift(*span, delta),
        },
        Expr::Binary { op, lhs, rhs, span } => Expr::Binary {
            op: *op,
            lhs: sub(lhs),
            rhs: sub(rhs),
            span: shift(*span, delta),
        },
        Expr::Call {
            callee,
            type_args,
            args,
            span,
        } => Expr::Call {
            callee: sub(callee),
            type_args: type_args.iter().map(|t| substitute_ann(t, bind)).collect(),
            args: args.iter().map(|a| clone_expr(a, bind, delta)).collect(),
            span: shift(*span, delta),
        },
        Expr::Field {
            base,
            name,
            optional,
            span,
        } => Expr::Field {
            base: sub(base),
            name: name.clone(),
            optional: *optional,
            span: shift(*span, delta),
        },
        Expr::StructLit {
            name,
            type_args,
            fields,
            span,
        } => Expr::StructLit {
            name: name.clone(),
            type_args: type_args.iter().map(|t| substitute_ann(t, bind)).collect(),
            fields: fields
                .iter()
                .map(|(n, v)| (n.clone(), clone_expr(v, bind, delta)))
                .collect(),
            span: shift(*span, delta),
        },
        Expr::EnumLit {
            name,
            type_args,
            variant,
            args,
            span,
        } => Expr::EnumLit {
            name: name.clone(),
            type_args: type_args.iter().map(|t| substitute_ann(t, bind)).collect(),
            variant: variant.clone(),
            args: args.iter().map(|a| clone_expr(a, bind, delta)).collect(),
            span: shift(*span, delta),
        },
        Expr::ArrayLit { elements, span } => Expr::ArrayLit {
            elements: elements
                .iter()
                .map(|e| clone_expr(e, bind, delta))
                .collect(),
            span: shift(*span, delta),
        },
        Expr::Index { base, index, span } => Expr::Index {
            base: sub(base),
            index: sub(index),
            span: shift(*span, delta),
        },
    }
}
