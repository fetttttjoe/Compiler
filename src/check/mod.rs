use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::narrow::{NarrowFrame, body_effects, covers, diverges, null_checks};
use crate::span::Span;
use crate::syntax;
use crate::types::{
    FnSig, StructType, Type, eq_comparable, fits, is_numeric, poisoned, unconstrained,
};

/// A per-module view: visible name → the (module, name) that defines it.
pub type Alias = HashMap<String, (usize, String)>;

/// Name-resolution results — the checker's durable output. For every module,
/// each callable name maps to its defining (module, name), locals and imports
/// alike. Consumed by the interpreter (and later codegen).
pub struct Resolutions {
    pub functions: Vec<Alias>,
    /// Per module: the visible type names that denote `refstruct`s, so the
    /// interpreter (and later codegen) knows which literals allocate a
    /// shared object instead of a value.
    pub ref_structs: Vec<HashSet<String>>,
    /// Every struct definition by (defining module, name) — the type
    /// exports ADR 0009 reserved for codegen (field layout and by_ref).
    pub structs: HashMap<(usize, String), StructType>,
    /// Field-access resolution for codegen: each `Field` expr's span maps
    /// to its base struct, field index, and field type. Spans are
    /// file-global offsets, so they key uniquely across the whole program.
    pub field_slots: HashMap<Span, FieldSlot>,
    /// Every function's resolved signature by (defining module, name).
    pub sigs: HashMap<(usize, String), FnSig>,
    /// The type of every checked expression, keyed by its span (spans are
    /// file-global offsets, unique program-wide). Narrowed where narrowing
    /// applies — which is exactly what codegen wants. Codegen never
    /// re-derives a type.
    pub expr_types: HashMap<Span, Type>,
    /// Each annotated `let`'s resolved declared type, keyed by the
    /// statement span — so codegen never resolves an annotation itself.
    pub let_types: HashMap<Span, Type>,
}

/// A resolved field access (see `Resolutions::field_slots`).
pub struct FieldSlot {
    pub base: (usize, String),
    pub index: usize,
    pub ty: Type,
}

/// One module's declared names with their export flags.
struct ModuleNames {
    fns: HashMap<String, bool>,
    structs: HashMap<String, bool>,
}

/// Static type checking over the whole module graph. Empty diagnostics =
/// well-typed program.
pub fn check(graph: &ModuleGraph) -> (Resolutions, Vec<Diagnostic>) {
    let mut diags = Vec::new();

    // Pass A: every module's own names + intra-file duplicate detection.
    let names: Vec<ModuleNames> = graph
        .modules
        .iter()
        .map(|m| collect_names(&m.ast, &mut diags))
        .collect();

    // Pass B: alias maps — locals plus imported bindings, with import errors
    // (unknown item, not exported, collision within this file).
    let mut fn_aliases: Vec<Alias> = Vec::new();
    let mut ty_aliases: Vec<Alias> = Vec::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        let mut fn_alias: Alias = names[mi]
            .fns
            .keys()
            .map(|n| (n.clone(), (mi, n.clone())))
            .collect();
        let mut ty_alias: Alias = names[mi]
            .structs
            .keys()
            .map(|n| (n.clone(), (mi, n.clone())))
            .collect();
        for binding in &module.imports {
            let target = &names[binding.target];
            let target_path = &graph.modules[binding.target].path;
            let fn_export = target.fns.get(&binding.name).copied();
            let ty_export = target.structs.get(&binding.name).copied();
            if fn_export.is_none() && ty_export.is_none() {
                let exported_names = target
                    .fns
                    .iter()
                    .chain(target.structs.iter())
                    .filter(|&(_, &exported)| exported)
                    .map(|(n, _)| n.as_str());
                diags.push(
                    Diagnostic::error(
                        format!("module '{target_path}' has no item '{}'", binding.name),
                        binding.span,
                    )
                    .suggest(&binding.name, exported_names),
                );
                continue;
            }
            if fn_export != Some(true) && ty_export != Some(true) {
                diags.push(
                    Diagnostic::error(
                        format!(
                            "'{}' exists in '{target_path}' but is not exported",
                            binding.name
                        ),
                        binding.span,
                    )
                    .with_help(format!(
                        "add 'export' before the definition of '{}' in '{target_path}'",
                        binding.name
                    )),
                );
                continue;
            }
            if fn_alias.contains_key(&binding.name) || ty_alias.contains_key(&binding.name) {
                diags.push(Diagnostic::error(
                    format!("'{}' is already defined in this file", binding.name),
                    binding.span,
                ));
                continue;
            }
            if fn_export == Some(true) {
                fn_alias.insert(binding.name.clone(), (binding.target, binding.name.clone()));
            }
            if ty_export == Some(true) {
                ty_alias.insert(binding.name.clone(), (binding.target, binding.name.clone()));
            }
        }
        fn_aliases.push(fn_alias);
        ty_aliases.push(ty_alias);
    }

    // Pass C: resolve signatures and struct layouts through the alias maps.
    let mut sigs: HashMap<(usize, String), FnSig> = HashMap::new();
    let mut structs: HashMap<(usize, String), StructType> = HashMap::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            match item {
                Item::Struct(s) => {
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                resolve_type(&f.ty, &ty_aliases[mi], s.span, &mut diags),
                            )
                        })
                        .collect();
                    structs.insert(
                        (mi, s.name.clone()),
                        StructType {
                            fields,
                            by_ref: s.by_ref,
                        },
                    );
                }
                Item::Function(f) => {
                    let params = f
                        .params
                        .iter()
                        .map(|p| resolve_type(&p.ty, &ty_aliases[mi], f.span, &mut diags))
                        .collect();
                    let ret = match &f.return_type {
                        Some(t) => resolve_type(t, &ty_aliases[mi], f.span, &mut diags),
                        None => Type::Unit,
                    };
                    sigs.insert((mi, f.name.clone()), FnSig { params, ret });
                }
                Item::Import(_) => {}
            }
        }
    }

    // Pass D: check every function body against its module's view.
    let paths: Vec<&str> = graph.modules.iter().map(|m| m.path.as_str()).collect();
    let mut field_slots = HashMap::new();
    let mut expr_types = HashMap::new();
    let mut let_types = HashMap::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                let mut checker = Checker {
                    module: mi,
                    paths: &paths,
                    fn_alias: &fn_aliases[mi],
                    ty_alias: &ty_aliases[mi],
                    sigs: &sigs,
                    structs: &structs,
                    diagnostics: &mut diags,
                    scopes: Vec::new(),
                    nonnull: Vec::new(),
                    loop_depth: 0,
                    ret: Type::Unit,
                    field_slots: &mut field_slots,
                    expr_types: &mut expr_types,
                    let_types: &mut let_types,
                };
                checker.check_function(f);
            }
        }
    }

    // Entry rule, owned by the checker so every entry path agrees: the
    // interpreter calls `main` with no arguments, and compiled main would
    // read argc/argv as its parameters.
    if let Some(f) = graph.modules[0].ast.iter().find_map(|item| match item {
        Item::Function(f) if f.name == syntax::ENTRY_FN => Some(f),
        _ => None,
    }) && !f.params.is_empty()
    {
        diags.push(Diagnostic::error(
            format!("'{}' takes no parameters", syntax::ENTRY_FN),
            f.span,
        ));
    }

    let ref_structs = ty_aliases
        .iter()
        .map(|alias| {
            alias
                .iter()
                .filter(|(_, key)| structs.get(*key).is_some_and(|s| s.by_ref))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .collect();

    (
        Resolutions {
            functions: fn_aliases,
            ref_structs,
            structs,
            field_slots,
            sigs,
            expr_types,
            let_types,
        },
        diags,
    )
}

fn collect_names(ast: &[Item], diags: &mut Vec<Diagnostic>) -> ModuleNames {
    let mut names = ModuleNames {
        fns: HashMap::new(),
        structs: HashMap::new(),
    };
    for item in ast {
        match item {
            Item::Function(f) => {
                if names.fns.insert(f.name.clone(), f.exported).is_some() {
                    diags.push(Diagnostic::error(
                        format!("function '{}' is already defined", f.name),
                        f.span,
                    ));
                }
            }
            Item::Struct(s) => {
                if names.structs.insert(s.name.clone(), s.exported).is_some() {
                    diags.push(Diagnostic::error(
                        format!("struct '{}' is already defined", s.name),
                        s.span,
                    ));
                }
            }
            Item::Import(_) => {}
        }
    }
    names
}

fn resolve_type(ann: &TypeAnn, ty_alias: &Alias, span: Span, diags: &mut Vec<Diagnostic>) -> Type {
    match ann {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Bool => Type::Bool,
        TypeAnn::Str => Type::Str,
        TypeAnn::Optional(inner) => {
            Type::Optional(Box::new(resolve_type(inner, ty_alias, span, diags)))
        }
        TypeAnn::Array(inner) => Type::Array(Box::new(resolve_type(inner, ty_alias, span, diags))),
        TypeAnn::Named(name) => match ty_alias.get(name) {
            Some((m, n)) => Type::Struct(*m, n.clone()),
            None => {
                diags.push(
                    Diagnostic::error(format!("unknown type '{name}'"), span)
                        .suggest(name, ty_alias.keys().map(String::as_str)),
                );
                Type::Error // recovery: already reported
            }
        },
    }
}
struct VarInfo {
    ty: Type,
    mutable: bool,
}

struct Checker<'a> {
    module: usize,
    paths: &'a [&'a str],
    fn_alias: &'a Alias,
    ty_alias: &'a Alias,
    sigs: &'a HashMap<(usize, String), FnSig>,
    structs: &'a HashMap<(usize, String), StructType>,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<HashMap<String, VarInfo>>,
    /// Narrowing stack: place paths (`cur`, `cur.left`) proven non-null by
    /// an enclosing `!= null` check. A narrowed `T?` reads as `T`. Rebinding
    /// removes a path and its extensions permanently; shadowing only hides
    /// it while the shadow's frame lives; field paths are also dropped on
    /// any call or field write, since aliases can reach them.
    nonnull: Vec<NarrowFrame>,
    /// How many loops enclose the statement being checked — `break`/
    /// `continue` are rejected at depth 0 (ADR 0019).
    loop_depth: usize,
    ret: Type,
    /// Codegen resolution tables filled in as expressions type (see
    /// `Resolutions::field_slots` / `struct_lits`).
    field_slots: &'a mut HashMap<Span, FieldSlot>,
    expr_types: &'a mut HashMap<Span, Type>,
    let_types: &'a mut HashMap<Span, Type>,
}

mod exprs;
mod stmts;
#[cfg(test)]
mod tests;

/// Shared plumbing: diagnostics, name rendering, and scope helpers.
impl<'a> Checker<'a> {
    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }

    /// A type name for messages: structs from *other* modules carry their
    /// defining file, so same-named types stay distinguishable
    /// (`P (from a.ys)` vs `P`).
    fn type_name(&self, t: &Type) -> String {
        match t {
            Type::Struct(m, n) if *m != self.module => {
                format!("{n} (from {})", self.paths[*m])
            }
            Type::Optional(inner) => format!("{}?", self.type_name(inner)),
            Type::Array(inner) if unconstrained(inner) => "[]".to_string(),
            Type::Array(inner) => format!("{}[]", self.type_name(inner)),
            _ => t.name(),
        }
    }
}

/// The variable at the root of a place expression (`o.i.v` → `o`).
fn root_ident(e: &Expr) -> Option<(&str, Span)> {
    match e {
        Expr::Ident(n, s) => Some((n, *s)),
        Expr::Field { base, .. } | Expr::Index { base, .. } => root_ident(base),
        _ => None,
    }
}

/// Definite-return analysis: does this statement list guarantee a `return` on
/// every path? An `if` guarantees it only when both branches exist and both
/// return; a `while` never does (its condition can be false on entry).
fn always_returns(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } => true,
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => always_returns(then_body) && always_returns(else_body),
        _ => false,
    })
}
