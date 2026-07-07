use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::diagnostic::{closest, Diagnostic};
use crate::modules::ModuleGraph;
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    /// A struct type identified by (defining module, name) — same-named
    /// structs in different modules are distinct types.
    Struct(usize, String),
    /// `T?` — T or null.
    Optional(Box<Type>),
    /// The type of the `null` literal; fits only into `T?` slots.
    Null,
    Unit,
}

impl Type {
    pub fn name(&self) -> String {
        match self {
            Type::Int => "int".to_string(),
            Type::Float => "float".to_string(),
            Type::Bool => "bool".to_string(),
            Type::Str => "string".to_string(),
            Type::Struct(_, n) => n.clone(),
            Type::Optional(inner) => format!("{}?", inner.name()),
            Type::Null => "null".to_string(),
            Type::Unit => "unit".to_string(),
        }
    }
}

/// Can a value of type `value` be stored where `target` is expected?
/// Exact match, or `T`/`null` into `T?`. Never an implicit conversion —
/// optionality is spelled in the target's type.
fn fits(value: &Type, target: &Type) -> bool {
    if value == target {
        return true;
    }
    match target {
        Type::Optional(inner) => {
            matches!(value, Type::Null) || value == inner.as_ref()
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructType {
    pub fields: Vec<(String, Type)>,
    /// True for `refstruct` declarations (reference semantics).
    pub by_ref: bool,
}

/// A per-module view: visible name → the (module, name) that defines it.
type Alias = HashMap<String, (usize, String)>;

/// Name-resolution results — the checker's durable output. For every module,
/// each callable name maps to its defining (module, name), locals and imports
/// alike. Consumed by the interpreter (and later codegen).
pub struct Resolutions {
    pub functions: Vec<Alias>,
    /// Per module: the visible type names that denote `refstruct`s, so the
    /// interpreter (and later codegen) knows which literals allocate a
    /// shared object instead of a value.
    pub ref_structs: Vec<HashSet<String>>,
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
                let mut diag = Diagnostic::error(
                    format!("module '{target_path}' has no item '{}'", binding.name),
                    binding.span,
                );
                let exported_names = target
                    .fns
                    .iter()
                    .chain(target.structs.iter())
                    .filter(|(_, &exported)| exported)
                    .map(|(n, _)| n.as_str());
                if let Some(suggestion) = closest(&binding.name, exported_names) {
                    diag = diag.with_help(format!("did you mean '{suggestion}'?"));
                }
                diags.push(diag);
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
                    ret: Type::Unit,
                };
                checker.check_function(f);
            }
        }
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

fn resolve_type(
    ann: &TypeAnn,
    ty_alias: &Alias,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) -> Type {
    match ann {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Bool => Type::Bool,
        TypeAnn::Str => Type::Str,
        TypeAnn::Optional(inner) => {
            Type::Optional(Box::new(resolve_type(inner, ty_alias, span, diags)))
        }
        TypeAnn::Named(name) => match ty_alias.get(name) {
            Some((m, n)) => Type::Struct(*m, n.clone()),
            None => {
                let mut diag = Diagnostic::error(format!("unknown type '{name}'"), span);
                if let Some(suggestion) = closest(name, ty_alias.keys().map(String::as_str)) {
                    diag = diag.with_help(format!("did you mean '{suggestion}'?"));
                }
                diags.push(diag);
                Type::Unit // recovery
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
    /// Narrowing stack: names proven non-null by an enclosing `!= null`
    /// check. A narrowed `T?` reads as `T`; rebinding or shadowing a name
    /// removes it from every set.
    nonnull: Vec<HashSet<String>>,
    ret: Type,
}

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
            _ => t.name(),
        }
    }

    fn check_function(&mut self, f: &Function) {
        let sig = self.sigs[&(self.module, f.name.clone())].clone();
        self.ret = sig.ret;
        let mut scope = HashMap::new();
        for (param, ty) in f.params.iter().zip(sig.params) {
            scope.insert(param.name.clone(), VarInfo { ty, mutable: false });
        }
        self.scopes.push(scope);
        for stmt in &f.body {
            self.check_stmt(stmt);
        }
        self.scopes.pop();

        if self.ret != Type::Unit && !always_returns(&f.body) {
            self.error(
                format!("not all paths in function '{}' return a value", f.name),
                f.span,
            );
        }
    }

    /// Type-checks a nested block in its own scope (bindings made inside die
    /// at the closing brace), with a set of names proven non-null for its
    /// duration.
    fn check_block_narrowed(&mut self, stmts: &[Stmt], nonnull: HashSet<String>) {
        self.nonnull.push(nonnull);
        self.scopes.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        self.scopes.pop();
        self.nonnull.pop();
    }

    fn is_nonnull(&self, name: &str) -> bool {
        self.nonnull.iter().any(|set| set.contains(name))
    }

    /// Drops `name` from every narrowing set — used when it's rebound or
    /// shadowed and might be null again.
    fn unnarrow(&mut self, name: &str) {
        for set in &mut self.nonnull {
            set.remove(name);
        }
    }

    fn check_condition(&mut self, keyword: &str, cond: &Expr) {
        let ty = self.type_of_expr(cond);
        if ty != Type::Bool {
            self.error(
                format!(
                    "{keyword} condition must be bool, found {}",
                    self.type_name(&ty)
                ),
                cond.span(),
            );
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let {
                mutable,
                name,
                ty,
                value,
                span,
            } => {
                let init_ty = self.type_of_expr(value);
                let ty = match ty {
                    Some(ann) => {
                        let declared =
                            resolve_type(ann, self.ty_alias, *span, self.diagnostics);
                        if !fits(&init_ty, &declared) {
                            self.error(
                                format!(
                                    "'{name}' is declared as {} but initialized with {}",
                                    self.type_name(&declared),
                                    self.type_name(&init_ty)
                                ),
                                *span,
                            );
                        }
                        declared
                    }
                    None if init_ty == Type::Null => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                format!("cannot infer a type for '{name}' from 'null'"),
                                *span,
                            )
                            .with_help(format!(
                                "add an annotation, e.g. 'var {name}: T? = null;'"
                            )),
                        );
                        Type::Unit // recovery
                    }
                    None => init_ty,
                };
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    VarInfo {
                        ty,
                        mutable: *mutable,
                    },
                );
                // A new binding shadows any narrowing of the same name.
                self.unnarrow(name);
            }
            Stmt::Return { value, span } => {
                let ty = match value {
                    Some(e) => self.type_of_expr(e),
                    None => Type::Unit,
                };
                if !fits(&ty, &self.ret) {
                    self.error(
                        format!(
                            "expected return type {}, found {}",
                            self.type_name(&self.ret.clone()),
                            self.type_name(&ty)
                        ),
                        *span,
                    );
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.check_condition("if", cond);
                let (if_true, if_false) = null_checks(cond);
                self.check_block_narrowed(then_body, if_true);
                if let Some(else_body) = else_body {
                    self.check_block_narrowed(else_body, if_false);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.check_condition("while", cond);
                let (if_true, _) = null_checks(cond);
                self.check_block_narrowed(body, if_true);
            }
            Stmt::Expr(e) => {
                self.type_of_expr(e);
            }
            Stmt::Assign { target, value, span } => {
                let value_ty = self.type_of_expr(value);
                // The parser only builds place targets, so a root always exists.
                let Some((root, root_span)) = root_ident(target) else {
                    return;
                };
                let Some(mutable) = self.find_var(root).map(|info| info.mutable) else {
                    self.lookup(root, root_span); // emits undefined + suggestion
                    return;
                };
                // Rebinding a variable invalidates its narrowing — the new
                // value may be null again. (The value above was typed while
                // still narrowed, so `cur = cur.next` checks out.)
                if matches!(target, Expr::Ident(..)) {
                    self.unnarrow(root);
                }
                // Typing the target may emit its own errors (unknown field);
                // stop here when it does — mutability/mismatch checks on an
                // ill-formed target would only add noise.
                let before = self.diagnostics.len();
                let target_ty = self.type_of_expr(target);
                if self.diagnostics.len() != before {
                    return;
                }
                // Mutation needs a `var` root unless the chain crosses a
                // refstruct boundary — past a reference we mutate the shared
                // object, not the binding.
                if !mutable && !self.crosses_ref(target) {
                    self.error(format!("cannot assign to const '{root}'"), *span);
                    return;
                }
                if !fits(&value_ty, &target_ty) {
                    let message = match target {
                        Expr::Field { name, .. } => format!(
                            "field '{name}' expects {}, found {}",
                            self.type_name(&target_ty),
                            self.type_name(&value_ty)
                        ),
                        _ => format!(
                            "cannot assign {} to variable of type {}",
                            self.type_name(&value_ty),
                            self.type_name(&target_ty)
                        ),
                    };
                    self.error(message, *span);
                }
            }
        }
    }

    fn type_of_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Int(_, _) => Type::Int,
            Expr::Float(_, _) => Type::Float,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Str(_, _) => Type::Str,
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Null(_) => Type::Null,
            Expr::Unary { op, rhs, span } => {
                let ty = self.type_of_expr(rhs);
                match op {
                    UnOp::Neg if is_numeric(&ty) => ty,
                    UnOp::Neg => {
                        self.error(format!("cannot negate {}", self.type_name(&ty)), *span);
                        ty
                    }
                    UnOp::Not if ty == Type::Bool => Type::Bool,
                    UnOp::Not => {
                        self.error(
                            format!("cannot apply '!' to {}", self.type_name(&ty)),
                            *span,
                        );
                        Type::Bool
                    }
                }
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let lt = self.type_of_expr(lhs);
                // `x != null && …` — the null check guards the right side.
                let rt = if *op == BinOp::And {
                    let (if_true, _) = null_checks(lhs);
                    self.nonnull.push(if_true);
                    let rt = self.type_of_expr(rhs);
                    self.nonnull.pop();
                    rt
                } else {
                    self.type_of_expr(rhs)
                };
                self.check_binary(*op, lt, rt, *span)
            }
            Expr::Call { callee, args, span } => self.check_call(callee, args, *span),
            Expr::Field {
                base,
                name,
                optional,
                span,
            } => self.check_field(base, name, *optional, *span),
            Expr::StructLit { name, fields, span } => self.check_struct_lit(name, fields, *span),
        }
    }

    fn check_binary(&mut self, op: BinOp, lt: Type, rt: Type, span: Span) -> Type {
        use BinOp::*;
        let (ok, result) = match op {
            // Arithmetic on matching numerics; `+` also concatenates strings.
            Add | Sub | Mul | Div | Rem => {
                let ok = lt == rt && (is_numeric(&lt) || (op == Add && lt == Type::Str));
                (ok, lt.clone())
            }
            // Ordering on matching numerics.
            Lt | Le | Gt | Ge => (lt == rt && is_numeric(&lt), Type::Bool),
            // Equality on any matching primitive or struct type (struct
            // identity is (module, name)), plus null checks on optionals.
            Eq | Ne => (eq_comparable(&lt, &rt), Type::Bool),
            // Logic on bools.
            And | Or => (lt == Type::Bool && rt == Type::Bool, Type::Bool),
            // `a ?? b`: a must be optional; b re-fills it (`T` unwraps,
            // `T?`/null keep it optional).
            Coalesce => match &lt {
                Type::Optional(inner) => {
                    let result = if rt == **inner {
                        (**inner).clone()
                    } else {
                        lt.clone()
                    };
                    (fits(&rt, &lt), result)
                }
                _ => (false, lt.clone()),
            },
        };
        if !ok {
            self.error(
                format!(
                    "cannot apply '{}' to {} and {}",
                    op.symbol(),
                    self.type_name(&lt),
                    self.type_name(&rt)
                ),
                span,
            );
        }
        result
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Type {
        let name = match callee {
            Expr::Ident(n, _) => n.clone(),
            _ => {
                self.error("only named functions can be called".to_string(), span);
                return Type::Unit;
            }
        };
        // Copy the map references out of `self` so the signature borrow is
        // independent of the `&mut self` calls below — no clone needed.
        let sigs = self.sigs;
        let Some(target) = self.fn_alias.get(&name) else {
            let mut diag = Diagnostic::error(format!("undefined function '{name}'"), span);
            if let Some(suggestion) = closest(&name, self.fn_alias.keys().map(String::as_str)) {
                diag = diag.with_help(format!("did you mean '{suggestion}'?"));
            }
            self.diagnostics.push(diag);
            // Still check the arguments — their own errors shouldn't vanish
            // just because the callee is unknown.
            for arg in args {
                self.type_of_expr(arg);
            }
            return Type::Unit;
        };
        let sig = &sigs[target];
        if args.len() != sig.params.len() {
            self.error(
                format!(
                    "function '{}' expects {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
                span,
            );
        }
        for (arg, expected) in args.iter().zip(&sig.params) {
            let got = self.type_of_expr(arg);
            if !fits(&got, expected) {
                self.error(
                    format!(
                        "expected argument of type {}, found {}",
                        self.type_name(expected),
                        self.type_name(&got)
                    ),
                    arg.span(),
                );
            }
        }
        sig.ret.clone()
    }

    fn check_field(&mut self, base: &Expr, field: &str, optional: bool, span: Span) -> Type {
        let base_ty = self.type_of_expr(base);
        // Stage 1: the optional layer — `?.` must strip one, `.` must not
        // have one to strip.
        let inner = match (&base_ty, optional) {
            (Type::Optional(inner), true) => inner.as_ref(),
            (Type::Optional(_), false) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "{} may be null — its fields can't be read directly",
                            self.type_name(&base_ty)
                        ),
                        span,
                    )
                    .with_help("use '?.', or check '!= null' first".to_string()),
                );
                return Type::Unit;
            }
            (_, true) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "'?.' on {}, which is never null",
                            self.type_name(&base_ty)
                        ),
                        span,
                    )
                    .with_help("use '.'".to_string()),
                );
                return Type::Unit;
            }
            (ty, false) => ty,
        };
        // Stage 2: the field lookup, shared by both forms.
        let Type::Struct(sm, struct_name) = inner else {
            self.error(
                format!("type {} has no fields", self.type_name(&base_ty)),
                span,
            );
            return Type::Unit;
        };
        let (sm, struct_name) = (*sm, struct_name.clone());
        let field_ty = self
            .structs
            .get(&(sm, struct_name.clone()))
            .and_then(|st| st.fields.iter().find(|(fname, _)| fname == field))
            .map(|(_, ty)| ty.clone());
        match field_ty {
            Some(ty) if optional => match ty {
                // `a?.b` is optional; an already-optional field stays flat.
                Type::Optional(_) => ty,
                other => Type::Optional(Box::new(other)),
            },
            Some(ty) => ty,
            None => {
                self.error(
                    format!("struct '{struct_name}' has no field '{field}'"),
                    span,
                );
                Type::Unit
            }
        }
    }

    fn check_struct_lit(&mut self, name: &str, fields: &[(String, Expr)], span: Span) -> Type {
        let structs = self.structs;
        let Some(key) = self.ty_alias.get(name) else {
            let mut diag = Diagnostic::error(format!("unknown struct '{name}'"), span);
            if let Some(suggestion) = closest(name, self.ty_alias.keys().map(String::as_str)) {
                diag = diag.with_help(format!("did you mean '{suggestion}'?"));
            }
            self.diagnostics.push(diag);
            for (_, value) in fields {
                self.type_of_expr(value);
            }
            return Type::Unit;
        };
        let decl = &structs[key];
        let mut seen = HashSet::new();
        for (fname, value) in fields {
            if !seen.insert(fname.clone()) {
                self.error(
                    format!("duplicate field '{fname}' in struct literal"),
                    value.span(),
                );
            }
            let got = self.type_of_expr(value);
            match decl.fields.iter().find(|(dn, _)| dn == fname) {
                Some((_, expected)) => {
                    if !fits(&got, expected) {
                        self.error(
                            format!(
                                "field '{fname}' expects {}, found {}",
                                self.type_name(expected),
                                self.type_name(&got)
                            ),
                            value.span(),
                        );
                    }
                }
                None => self.error(
                    format!("struct '{name}' has no field '{fname}'"),
                    value.span(),
                ),
            }
        }
        for (dn, _) in &decl.fields {
            if !fields.iter().any(|(fname, _)| fname == dn) {
                self.error(format!("missing field '{dn}' in struct '{name}'"), span);
            }
        }
        Type::Struct(key.0, key.1.clone())
    }

    /// True when mutating through this place chain passes a refstruct
    /// boundary (some field access has a by-ref base). Bases get re-typed,
    /// but only after the whole target typed cleanly, so no duplicate
    /// diagnostics are emitted.
    fn crosses_ref(&mut self, place: &Expr) -> bool {
        match place {
            Expr::Field { base, .. } => {
                let base_ty = self.type_of_expr(base);
                self.is_by_ref(&base_ty) || self.crosses_ref(base)
            }
            _ => false,
        }
    }

    fn is_by_ref(&self, t: &Type) -> bool {
        match t {
            Type::Struct(m, n) => self
                .structs
                .get(&(*m, n.clone()))
                .is_some_and(|s| s.by_ref),
            _ => false,
        }
    }

    /// The innermost binding for `name`, searching scopes inside-out.
    fn find_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn lookup(&mut self, name: &str, span: Span) -> Type {
        if let Some(info) = self.find_var(name) {
            // A narrowed optional reads as its inner type.
            if self.is_nonnull(name) {
                if let Type::Optional(inner) = &info.ty {
                    return (**inner).clone();
                }
            }
            return info.ty.clone();
        }
        let visible = self.scopes.iter().flat_map(|s| s.keys().map(String::as_str));
        let mut diag = Diagnostic::error(format!("undefined variable '{name}'"), span);
        if let Some(suggestion) = closest(name, visible) {
            diag = diag.with_help(format!("did you mean '{suggestion}'?"));
        }
        self.diagnostics.push(diag);
        Type::Unit
    }
}

/// Comparability for `==`/`!=`: two non-unit types compare when either fits
/// the other — same type, `null` vs `T?`, or `T` vs `T?`.
fn eq_comparable(lt: &Type, rt: &Type) -> bool {
    *lt != Type::Unit && *rt != Type::Unit && (fits(lt, rt) || fits(rt, lt))
}

/// The names a condition proves non-null: `(if_true, if_false)`.
/// `x != null` proves `x` in the true branch, `x == null` in the false
/// branch; `&&` accumulates its sides' true-facts. Nothing deeper (v1).
fn null_checks(cond: &Expr) -> (HashSet<String>, HashSet<String>) {
    let (mut if_true, mut if_false) = (HashSet::new(), HashSet::new());
    if let Expr::Binary { op, lhs, rhs, .. } = cond {
        match op {
            BinOp::Ne => if_true.extend(ident_vs_null(lhs, rhs)),
            BinOp::Eq => if_false.extend(ident_vs_null(lhs, rhs)),
            BinOp::And => {
                if_true.extend(null_checks(lhs).0);
                if_true.extend(null_checks(rhs).0);
            }
            _ => {}
        }
    }
    (if_true, if_false)
}

fn ident_vs_null(a: &Expr, b: &Expr) -> Option<String> {
    match (a, b) {
        (Expr::Ident(n, _), Expr::Null(_)) | (Expr::Null(_), Expr::Ident(n, _)) => {
            Some(n.clone())
        }
        _ => None,
    }
}

/// The variable at the root of a place expression (`o.i.v` → `o`).
fn root_ident(e: &Expr) -> Option<(&str, Span)> {
    match e {
        Expr::Ident(n, s) => Some((n, *s)),
        Expr::Field { base, .. } => root_ident(base),
        _ => None,
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{load_program, Module};
    use crate::source::SourceMap;
    use crate::{lexer::lex, parser::parse};

    fn graph_of(src: &str) -> ModuleGraph {
        let (tokens, ld) = lex(src);
        assert!(ld.is_empty(), "lex: {ld:?}");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse: {pd:?}");
        ModuleGraph {
            modules: vec![Module {
                path: "test.ys".to_string(),
                ast,
                imports: Vec::new(),
            }],
        }
    }

    /// Checks a multi-file program (first file = entry) through the real
    /// module loader.
    fn multi(files: &[(&str, &str)]) -> (Resolutions, Vec<Diagnostic>) {
        let store: HashMap<String, String> = files
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut read = |p: &str| {
            store
                .get(p)
                .cloned()
                .ok_or_else(|| "no such file".to_string())
        };
        let mut map = SourceMap::new();
        let (graph, fd) = load_program(files[0].0, &mut read, &mut map).unwrap();
        assert!(fd.is_empty(), "front-end: {fd:?}");
        check(&graph)
    }

    #[test]
    fn resolves_local_names_to_the_defining_module() {
        let (res, d) = check(&graph_of(
            "struct P { x: int } fun f(a: int): float { return 1.0; }",
        ));
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
        assert_eq!(res.functions[0]["f"], (0, "f".to_string()));
    }

    #[test]
    fn duplicate_function_is_an_error() {
        let d = diags("fun f(): int { return 1; } fun f(): int { return 2; }");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("already defined"));
    }

    #[test]
    fn unknown_type_is_an_error() {
        let (_, d) = check(&graph_of("fun f(a: Missing): int { return 1; }"));
        assert!(d.iter().any(|e| e.message.contains("unknown type 'Missing'")));
    }

    fn diags(src: &str) -> Vec<Diagnostic> {
        check(&graph_of(src)).1
    }

    #[test]
    fn arithmetic_of_same_type_is_ok() {
        let d = diags("fun f(a: int, b: int): int { return a + b * 2; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn mixing_int_and_float_is_an_error() {
        let d = diags("fun f(a: int): int { const x = a + 1.0; return a; }");
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '+'")),
            "{d:?}"
        );
    }

    #[test]
    fn undefined_variable_is_an_error() {
        let d = diags("fun f(): int { return missing; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("undefined variable 'missing'")),
            "{d:?}"
        );
    }

    #[test]
    fn comparisons_equality_and_logic_are_well_typed() {
        let d = diags(
            "fun f(a: int, b: int): bool { return a < b && a != b || !(a >= b); }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn logical_operators_require_bools() {
        let d = diags("fun f(a: int, b: int): bool { return a && b; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot apply '&&' to int and int")),
            "{d:?}"
        );
    }

    #[test]
    fn equality_requires_matching_types() {
        let d = diags("fun f(): bool { return 1 == 1.0; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot apply '==' to int and float")),
            "{d:?}"
        );
    }

    #[test]
    fn struct_equality_is_well_typed() {
        let d = diags(
            "struct P { x: int }\n\
             fun f(a: P, b: P): bool { return a == b || a != b; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn struct_ordering_is_rejected() {
        let d = diags(
            "struct P { x: int }\n\
             fun f(a: P, b: P): bool { return a < b; }",
        );
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '<'")),
            "{d:?}"
        );
    }

    #[test]
    fn equality_across_distinct_struct_types_is_rejected() {
        // Same short name in two modules — identity is (module, name).
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { make } from \"./lib\";\n\
                 struct P { x: int }\n\
                 fun f(): bool { return P { x: 1 } == make(); }",
            ),
            (
                "lib.ys",
                "export struct P { x: int }\n\
                 export fun make(): P { return P { x: 1 }; }",
            ),
        ]);
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '=='")),
            "{d:?}"
        );
    }

    #[test]
    fn not_requires_bool() {
        let d = diags("fun f(a: int): bool { return !a; }");
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '!' to int")),
            "{d:?}"
        );
    }

    #[test]
    fn string_concat_is_well_typed_and_mixed_concat_is_not() {
        let d = diags("fun f(s: string): string { return s + \"!\"; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
        let d = diags("fun f(s: string): string { return s + 1; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot apply '+' to string and int")),
            "{d:?}"
        );
    }

    #[test]
    fn if_and_while_conditions_must_be_bool() {
        let d = diags("fun f(a: int): int { if a { return 1; } return 0; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("if condition must be bool, found int")),
            "{d:?}"
        );
        let d = diags("fun f(a: int): int { while a { a = a - 1; } return a; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("while condition must be bool, found int")),
            "{d:?}"
        );
    }

    #[test]
    fn block_bindings_do_not_escape_their_scope() {
        let d = diags("fun f(a: bool): int { if a { const x = 1; } return x; }");
        assert!(
            d.iter().any(|e| e.message.contains("undefined variable 'x'")),
            "{d:?}"
        );
    }

    #[test]
    fn missing_return_paths_are_errors() {
        // Empty body.
        let d = diags("fun f(): int { }");
        assert!(
            d.iter().any(|e| e.message.contains("not all paths in function 'f' return")),
            "{d:?}"
        );
        // If without else can fall through.
        let d = diags("fun f(a: bool): int { if a { return 1; } }");
        assert!(
            d.iter().any(|e| e.message.contains("not all paths")),
            "{d:?}"
        );
        // While may run zero times.
        let d = diags("fun f(): int { while true { return 1; } }");
        assert!(
            d.iter().any(|e| e.message.contains("not all paths")),
            "{d:?}"
        );
    }

    #[test]
    fn both_branches_returning_satisfies_definite_return() {
        let d = diags("fun f(a: bool): int { if a { return 1; } else { return 2; } }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn imported_functions_and_structs_are_usable() {
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { make, Point } from \"./geo\";\n\
                 fun main(): int { const p = make(); const q = Point { x: p.x }; return q.x; }",
            ),
            (
                "geo.ys",
                "export struct Point { x: int }\n\
                 export fun make(): Point { return Point { x: 7 }; }",
            ),
        ]);
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn unknown_and_unexported_imports_have_distinct_errors() {
        let (_, d) = multi(&[
            ("main.ys", "import { nope } from \"./lib\"; fun main(): int { return 1; }"),
            ("lib.ys", "fun hidden(): int { return 1; }"),
        ]);
        assert!(
            d.iter().any(|e| e.message.contains("has no item 'nope'")),
            "{d:?}"
        );

        let (_, d) = multi(&[
            ("main.ys", "import { hidden } from \"./lib\"; fun main(): int { return 1; }"),
            ("lib.ys", "fun hidden(): int { return 1; }"),
        ]);
        assert!(
            d.iter()
                .any(|e| e.message.contains("'hidden' exists in 'lib.ys' but is not exported")),
            "{d:?}"
        );
    }

    #[test]
    fn import_collisions_within_one_file_are_errors() {
        // Import colliding with a local definition.
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { f } from \"./lib\";\nfun f(): int { return 2; }\nfun main(): int { return f(); }",
            ),
            ("lib.ys", "export fun f(): int { return 1; }"),
        ]);
        assert!(
            d.iter()
                .any(|e| e.message.contains("'f' is already defined in this file")),
            "{d:?}"
        );

        // Import colliding with another import.
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { f } from \"./a\"; import { f } from \"./b\"; fun main(): int { return f(); }",
            ),
            ("a.ys", "export fun f(): int { return 1; }"),
            ("b.ys", "export fun f(): int { return 2; }"),
        ]);
        assert!(
            d.iter()
                .any(|e| e.message.contains("'f' is already defined in this file")),
            "{d:?}"
        );
    }

    #[test]
    fn same_names_in_different_modules_coexist() {
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { a } from \"./a\"; import { b } from \"./b\";\n\
                 fun main(): int { return a() + b(); }",
            ),
            ("a.ys", "fun helper(): int { return 1; } export fun a(): int { return helper(); }"),
            ("b.ys", "fun helper(): int { return 2; } export fun b(): int { return helper(); }"),
        ]);
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn same_named_structs_in_different_modules_are_distinct_types() {
        let (_, d) = multi(&[
            (
                "main.ys",
                "import { make } from \"./a\"; import { take } from \"./b\";\n\
                 fun main(): int { return take(make()); }",
            ),
            (
                "a.ys",
                "export struct P { x: int }\nexport fun make(): P { return P { x: 1 }; }",
            ),
            (
                "b.ys",
                "export struct P { x: int }\nexport fun take(p: P): int { return p.x; }",
            ),
        ]);
        // a.P and b.P share a name but are different types — and the message
        // says where each one lives.
        assert!(
            d.iter().any(|e| e
                .message
                .contains("expected argument of type P (from b.ys), found P (from a.ys)")),
            "{d:?}"
        );
    }

    #[test]
    fn undefined_names_suggest_close_matches() {
        let d = diags("fun f(): int { const account = 1; return acount; }");
        assert!(
            d.iter().any(|e| e.message.contains("undefined variable 'acount'")
                && e.help.as_deref() == Some("did you mean 'account'?")),
            "{d:?}"
        );

        let d = diags(
            "fun fibonacci(n: int): int { return n; }\n\
             fun main(): int { const answer = 1; return fibonaci(answr); }",
        );
        assert!(
            d.iter().any(|e| e.message.contains("undefined function 'fibonaci'")
                && e.help.as_deref() == Some("did you mean 'fibonacci'?")),
            "{d:?}"
        );
        // Arguments are still checked even when the callee is unknown.
        assert!(
            d.iter().any(|e| e.message.contains("undefined variable 'answr'")
                && e.help.as_deref() == Some("did you mean 'answer'?")),
            "{d:?}"
        );

        let d = diags("struct Point { x: int } fun f(): int { const p = Pont { x: 1 }; return 1; }");
        assert!(
            d.iter().any(|e| e.message.contains("unknown struct 'Pont'")
                && e.help.as_deref() == Some("did you mean 'Point'?")),
            "{d:?}"
        );
    }

    #[test]
    fn unexported_import_gets_an_export_hint() {
        let (_, d) = multi(&[
            ("main.ys", "import { hidden } from \"./lib\"; fun main(): int { return 1; }"),
            ("lib.ys", "fun hidden(): int { return 1; }"),
        ]);
        assert!(
            d.iter().any(|e| e.message.contains("is not exported")
                && e.help
                    .as_deref()
                    .is_some_and(|h| h.contains("add 'export' before the definition of 'hidden'"))),
            "{d:?}"
        );
    }

    #[test]
    fn misspelled_import_suggests_an_exported_name() {
        let (_, d) = multi(&[
            ("main.ys", "import { doubel } from \"./lib\"; fun main(): int { return 1; }"),
            ("lib.ys", "export fun double(n: int): int { return n * 2; }"),
        ]);
        assert!(
            d.iter().any(|e| e.message.contains("has no item 'doubel'")
                && e.help.as_deref() == Some("did you mean 'double'?")),
            "{d:?}"
        );
    }

    #[test]
    fn unit_functions_need_no_return() {
        let d = diags("fun f(a: int) { f(a - 1); }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn return_type_mismatch_is_an_error() {
        let d = diags("fun f(): int { return 1.0; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("expected return type int")),
            "{d:?}"
        );
    }

    #[test]
    fn well_typed_program_passes() {
        let d = diags(
            "struct P { x: int, y: int }\n\
             fun add(a: int, b: int): int { return a + b; }\n\
             fun main(): int { const p = P { x: 1, y: 2 }; return add(p.x, p.y); }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn wrong_argument_count_is_an_error() {
        let d = diags("fun f(a: int): int { return a; } fun g(): int { return f(); }");
        assert!(
            d.iter().any(|e| e.message.contains("expects 1 argument")),
            "{d:?}"
        );
    }

    #[test]
    fn argument_type_mismatch_is_an_error() {
        let d = diags("fun f(a: int): int { return a; } fun g(): int { return f(1.0); }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("expected argument of type int")),
            "{d:?}"
        );
    }

    #[test]
    fn unknown_field_is_an_error() {
        let d = diags("struct P { x: int } fun f(p: P): int { return p.z; }");
        assert!(d.iter().any(|e| e.message.contains("no field 'z'")), "{d:?}");
    }

    #[test]
    fn struct_literal_field_type_mismatch_is_an_error() {
        let d = diags("struct P { x: int } fun f(): int { const p = P { x: 1.0 }; return 1; }");
        assert!(
            d.iter().any(|e| e.message.contains("field 'x' expects int")),
            "{d:?}"
        );
    }

    #[test]
    fn duplicate_struct_literal_field_is_an_error() {
        let d = diags(
            "struct P { x: int } fun f(): int { const p = P { x: 1, x: 2 }; return 1; }",
        );
        assert!(
            d.iter().any(|e| e.message.contains("duplicate field 'x'")),
            "{d:?}"
        );
    }

    #[test]
    fn assign_type_mismatch_is_an_error() {
        let d = diags("fun f(): int { var x = 1; x = 1.0; return x; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign float to variable of type int")),
            "{d:?}"
        );
    }

    #[test]
    fn missing_struct_literal_field_is_an_error() {
        let d = diags(
            "struct P { x: int, y: int } fun f(): int { const p = P { x: 1 }; return 1; }",
        );
        assert!(
            d.iter()
                .any(|e| e.message.contains("missing field 'y' in struct 'P'")),
            "{d:?}"
        );
    }

    #[test]
    fn field_assignment_is_well_typed() {
        let d = diags("struct P { x: int } fun f() { var p = P { x: 1 }; p.x = 2; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn field_assignment_type_mismatch_is_an_error() {
        let d = diags("struct P { x: int } fun f() { var p = P { x: 1 }; p.x = 1.0; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("field 'x' expects int, found float")),
            "{d:?}"
        );
    }

    #[test]
    fn field_assignment_through_const_is_an_error() {
        let d = diags("struct P { x: int } fun f() { const p = P { x: 1 }; p.x = 2; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'p'")),
            "{d:?}"
        );
    }

    #[test]
    fn assigning_to_unknown_field_is_an_error() {
        let d = diags("struct P { x: int } fun f() { var p = P { x: 1 }; p.z = 2; }");
        assert!(
            d.iter().any(|e| e.message.contains("no field 'z'")),
            "{d:?}"
        );
    }

    #[test]
    fn refstruct_field_mutation_through_const_is_allowed() {
        let d = diags("refstruct P { x: int } fun f() { const p = P { x: 1 }; p.x = 2; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn refstruct_param_mutation_is_allowed() {
        let d = diags("refstruct P { x: int } fun g(p: P) { p.x = 5; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn rebinding_const_refstruct_is_rejected() {
        let d = diags(
            "refstruct P { x: int } fun f() { const p = P { x: 1 }; p = P { x: 2 }; }",
        );
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'p'")),
            "{d:?}"
        );
    }

    #[test]
    fn value_struct_param_mutation_stays_rejected() {
        let d = diags("struct V { x: int } fun g(v: V) { v.x = 5; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'v'")),
            "{d:?}"
        );
    }

    #[test]
    fn mutation_past_a_ref_boundary_in_a_const_chain_is_allowed() {
        // `b` is a const value struct, but `b.r.v` mutates the shared R
        // object, not `b` itself — allowed. Replacing `b.r` would mutate
        // `b`'s own copy — rejected.
        let src = "refstruct R { v: int }\n\
                   struct Box { r: R }\n\
                   fun f() { const b = Box { r: R { v: 1 } }; b.r.v = 7; }";
        let d = diags(src);
        assert!(d.is_empty(), "unexpected: {d:?}");

        let src = "refstruct R { v: int }\n\
                   struct Box { r: R }\n\
                   fun f() { const b = Box { r: R { v: 1 } }; b.r = R { v: 2 }; }";
        let d = diags(src);
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'b'")),
            "{d:?}"
        );
    }

    #[test]
    fn refstruct_equality_is_well_typed() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(a: P, b: P): bool { return a == b || a != b; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn optional_annotation_accepts_null_and_values() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f() { var p: P? = null; p = P { x: 1 }; p = null; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn bare_null_needs_an_annotation() {
        let d = diags("fun f() { var x = null; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot infer") && e.help.is_some()),
            "{d:?}"
        );
    }

    #[test]
    fn plain_dot_on_an_optional_errors_with_a_hint() {
        let d = diags("refstruct P { x: int } fun f(p: P?): int { return p.x; }");
        assert!(
            d.iter().any(|e| e.message.contains("may be null")
                && e.help.as_deref().is_some_and(|h| h.contains("?."))),
            "{d:?}"
        );
    }

    #[test]
    fn optional_chaining_produces_an_optional() {
        let d = diags("refstruct P { x: int } fun f(p: P?): int? { return p?.x; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn optional_chaining_on_a_non_optional_is_rejected() {
        let d = diags("refstruct P { x: int } fun f(p: P): int? { return p?.x; }");
        assert!(
            d.iter().any(|e| e.message.contains("never null")),
            "{d:?}"
        );
    }

    #[test]
    fn coalescing_unwraps_an_optional() {
        let d = diags("refstruct P { x: int } fun f(p: P?): int { return p?.x ?? 0; }");
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn coalescing_requires_an_optional_left_side() {
        let d = diags("fun f(a: int): int { return a ?? 1; }");
        assert!(d.iter().any(|e| e.message.contains("'??'")), "{d:?}");
    }

    #[test]
    fn null_checks_narrow_in_if_and_while() {
        let d = diags(
            "refstruct Node { v: int, next: Node? }\n\
             fun sum(head: Node?): int {\n\
                 var acc = 0;\n\
                 var cur = head;\n\
                 while cur != null { acc = acc + cur.v; cur = cur.next; }\n\
                 if head != null { acc = acc + head.v; }\n\
                 return acc;\n\
             }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn leading_null_check_narrows_the_rest_of_the_condition() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?): bool { return p != null && p.x > 0; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn equals_null_narrows_the_else_branch() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?): int { if p == null { return 0; } else { return p.x; } }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn null_comparison_on_a_non_optional_is_rejected() {
        let d = diags("fun f(a: int): bool { return a == null; }");
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '=='")),
            "{d:?}"
        );
    }

    #[test]
    fn values_fit_optional_parameters() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?) { }\n\
             fun g() { f(P { x: 1 }); f(null); }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn struct_literal_fields_accept_null_for_optionals() {
        let d = diags(
            "refstruct Node { v: int, next: Node? }\n\
             fun f() { const n = Node { v: 1, next: null }; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    // --- Narrowing edges: what v1 must NOT accept, pinned deliberately ---

    #[test]
    fn or_conditions_do_not_narrow() {
        // If the left of `||` is false, p IS null — narrowing the right
        // side would be unsound.
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?): bool { return p != null || p.x > 0; }",
        );
        assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
    }

    #[test]
    fn early_return_does_not_narrow_after_the_if() {
        // v1 ceiling: `if p == null { return 0; }` doesn't narrow the
        // statements after the if — only an else branch is narrowed.
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?): int { if p == null { return 0; } return p.x; }",
        );
        assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
    }

    #[test]
    fn reassignment_inside_a_narrowed_block_unnarrows() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(q: P?): int {\n\
                 var p = q;\n\
                 if p != null { p = null; return p.x; }\n\
                 return 0;\n\
             }",
        );
        assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
    }

    #[test]
    fn shadowing_inside_a_narrowed_block_unnarrows() {
        let d = diags(
            "refstruct P { x: int }\n\
             fun get(): P? { return null; }\n\
             fun f(p: P?): int {\n\
                 if p != null { const p = get(); return p.x; }\n\
                 return 0;\n\
             }",
        );
        assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
    }

    #[test]
    fn while_conditions_narrow_through_extra_checks() {
        let d = diags(
            "refstruct Node { v: int, next: Node? }\n\
             fun f(head: Node?): int {\n\
                 var cur = head;\n\
                 var n = 0;\n\
                 while cur != null && cur.v < 5 { n = n + 1; cur = cur.next; }\n\
                 return n;\n\
             }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn narrowed_value_type_optionals_support_arithmetic() {
        let d = diags(
            "fun f(x: int?): int { if x != null { return x + 1; } else { return 0; } }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    // --- Optional-chaining edges ---

    #[test]
    fn optional_chaining_flattens_already_optional_fields() {
        // n?.next is Node?, not Node?? — there is no double optional.
        let d = diags(
            "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node?): Node? { return n?.next; }",
        );
        assert!(d.is_empty(), "unexpected: {d:?}");
    }

    #[test]
    fn plain_dot_into_an_optional_field_errors_with_a_hint() {
        // n.next is fine to read, but chaining `.v` through it is not.
        let d = diags(
            "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node): int { return n.next.v; }",
        );
        assert!(
            d.iter().any(|e| e.message.contains("may be null")
                && e.help.as_deref().is_some_and(|h| h.contains("?."))),
            "{d:?}"
        );
    }

    #[test]
    fn coalescing_an_already_unwrapped_value_is_rejected() {
        // (a ?? 1) is int; a second ?? has nothing left to unwrap.
        let d = diags("fun f(a: int?): int { return a ?? 1 ?? 9; }");
        assert!(
            d.iter().any(|e| e.message.contains("cannot apply '??'")),
            "{d:?}"
        );
    }

    #[test]
    fn assigning_to_const_is_an_error() {
        let d = diags("fun f(): int { const x = 1; x = 2; return x; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'x'")),
            "{d:?}"
        );
    }
}
