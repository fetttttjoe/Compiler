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
            Type::Unit => "unit".to_string(),
        }
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
}

/// A per-module view: visible name → the (module, name) that defines it.
type Alias = HashMap<String, (usize, String)>;

/// Name-resolution results — the checker's durable output. For every module,
/// each callable name maps to its defining (module, name), locals and imports
/// alike. Consumed by the interpreter (and later codegen).
pub struct Resolutions {
    pub functions: Vec<Alias>,
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
                    structs.insert((mi, s.name.clone()), StructType { fields });
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
                    ret: Type::Unit,
                };
                checker.check_function(f);
            }
        }
    }

    (Resolutions { functions: fn_aliases }, diags)
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

    /// Type-checks a nested block in its own scope: bindings made inside die
    /// at the closing brace.
    fn check_block(&mut self, stmts: &[Stmt]) {
        self.scopes.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        self.scopes.pop();
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
                value,
                ..
            } => {
                let ty = self.type_of_expr(value);
                self.scopes.last_mut().unwrap().insert(
                    name.clone(),
                    VarInfo {
                        ty,
                        mutable: *mutable,
                    },
                );
            }
            Stmt::Return { value, span } => {
                let ty = match value {
                    Some(e) => self.type_of_expr(e),
                    None => Type::Unit,
                };
                if ty != self.ret {
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
                self.check_block(then_body);
                if let Some(else_body) = else_body {
                    self.check_block(else_body);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.check_condition("while", cond);
                self.check_block(body);
            }
            Stmt::Expr(e) => {
                self.type_of_expr(e);
            }
            Stmt::Assign { name, value, span } => {
                let value_ty = self.type_of_expr(value);
                let binding = self
                    .find_var(name)
                    .map(|info| (info.ty.clone(), info.mutable));
                match binding {
                    None => self.error(format!("undefined variable '{name}'"), *span),
                    Some((binding_ty, mutable)) => {
                        if !mutable {
                            self.error(format!("cannot assign to const '{name}'"), *span);
                        } else if value_ty != binding_ty {
                            self.error(
                                format!(
                                    "cannot assign {} to variable of type {}",
                                    self.type_name(&value_ty),
                                    self.type_name(&binding_ty)
                                ),
                                *span,
                            );
                        }
                    }
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
                let rt = self.type_of_expr(rhs);
                self.check_binary(*op, lt, rt, *span)
            }
            Expr::Call { callee, args, span } => self.check_call(callee, args, *span),
            Expr::Field { base, name, span } => self.check_field(base, name, *span),
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
            // Equality on any matching primitive type.
            Eq | Ne => (lt == rt && is_primitive(&lt), Type::Bool),
            // Logic on bools.
            And | Or => (lt == Type::Bool && rt == Type::Bool, Type::Bool),
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
            if got != *expected {
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

    fn check_field(&mut self, base: &Expr, field: &str, span: Span) -> Type {
        let base_ty = self.type_of_expr(base);
        let (sm, struct_name) = match &base_ty {
            Type::Struct(m, n) => (*m, n.clone()),
            _ => {
                self.error(
                    format!("type {} has no fields", self.type_name(&base_ty)),
                    span,
                );
                return Type::Unit;
            }
        };
        let field_ty = self
            .structs
            .get(&(sm, struct_name.clone()))
            .and_then(|st| st.fields.iter().find(|(fname, _)| fname == field))
            .map(|(_, ty)| ty.clone());
        match field_ty {
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
                    if got != *expected {
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

    /// The innermost binding for `name`, searching scopes inside-out.
    fn find_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn lookup(&mut self, name: &str, span: Span) -> Type {
        if let Some(info) = self.find_var(name) {
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

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float)
}

fn is_primitive(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float | Type::Bool | Type::Str)
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
    fn assigning_to_const_is_an_error() {
        let d = diags("fun f(): int { const x = 1; x = 2; return x; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'x'")),
            "{d:?}"
        );
    }
}
