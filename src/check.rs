use std::collections::{HashMap, HashSet};

use crate::ast::{Ast, BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::diagnostic::Diagnostic;
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Struct(String),
    Unit,
}

impl Type {
    pub fn name(&self) -> String {
        match self {
            Type::Int => "int".to_string(),
            Type::Float => "float".to_string(),
            Type::Struct(n) => n.clone(),
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

#[derive(Debug, Default)]
pub struct SymbolTable {
    pub functions: HashMap<String, FnSig>,
    pub structs: HashMap<String, StructType>,
}

/// Static type checking. Returns the resolved symbol table (used later by
/// codegen) and every type error found. Empty diagnostics = well-typed.
pub fn check(ast: &Ast) -> (SymbolTable, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let table = collect_signatures(ast, &mut diagnostics);
    for item in ast {
        if let Item::Function(f) = item {
            let mut checker = Checker::new(&table, &mut diagnostics);
            checker.check_function(f);
        }
    }
    (table, diagnostics)
}

fn collect_signatures(ast: &Ast, diags: &mut Vec<Diagnostic>) -> SymbolTable {
    // First gather declared struct names so `Named` types can be resolved
    // regardless of declaration order.
    let struct_names: HashSet<String> = ast
        .iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some(s.name.clone()),
            _ => None,
        })
        .collect();

    let mut table = SymbolTable::default();
    for item in ast {
        match item {
            Item::Struct(s) => {
                let fields = s
                    .fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.clone(),
                            resolve_type(&f.ty, &struct_names, s.span, diags),
                        )
                    })
                    .collect();
                // `insert` returns the previous value if the name was already defined.
                if table
                    .structs
                    .insert(s.name.clone(), StructType { fields })
                    .is_some()
                {
                    diags.push(Diagnostic::error(
                        format!("struct '{}' is already defined", s.name),
                        s.span,
                    ));
                }
            }
            Item::Function(f) => {
                let params = f
                    .params
                    .iter()
                    .map(|p| resolve_type(&p.ty, &struct_names, f.span, diags))
                    .collect();
                let ret = match &f.return_type {
                    Some(t) => resolve_type(t, &struct_names, f.span, diags),
                    None => Type::Unit,
                };
                if table
                    .functions
                    .insert(f.name.clone(), FnSig { params, ret })
                    .is_some()
                {
                    diags.push(Diagnostic::error(
                        format!("function '{}' is already defined", f.name),
                        f.span,
                    ));
                }
            }
        }
    }
    table
}

fn resolve_type(
    ann: &TypeAnn,
    structs: &HashSet<String>,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) -> Type {
    match ann {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Named(name) => {
            if structs.contains(name) {
                Type::Struct(name.clone())
            } else {
                diags.push(Diagnostic::error(format!("unknown type '{name}'"), span));
                Type::Unit // recovery
            }
        }
    }
}

struct VarInfo {
    ty: Type,
    mutable: bool,
}

struct Checker<'a> {
    table: &'a SymbolTable,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<HashMap<String, VarInfo>>,
    ret: Type,
}

impl<'a> Checker<'a> {
    fn new(table: &'a SymbolTable, diagnostics: &'a mut Vec<Diagnostic>) -> Self {
        Checker {
            table,
            diagnostics,
            scopes: Vec::new(),
            ret: Type::Unit,
        }
    }

    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }

    fn check_function(&mut self, f: &Function) {
        let sig = self.table.functions[&f.name].clone();
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
                            self.ret.name(),
                            ty.name()
                        ),
                        *span,
                    );
                }
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
                                    value_ty.name(),
                                    binding_ty.name()
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
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Unary { op, rhs, span } => {
                let ty = self.type_of_expr(rhs);
                match op {
                    UnOp::Neg if is_numeric(&ty) => ty,
                    UnOp::Neg => {
                        self.error(format!("cannot negate {}", ty.name()), *span);
                        ty
                    }
                    UnOp::Not => {
                        self.error("operator '!' is not yet supported".to_string(), *span);
                        Type::Int
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
        match op {
            BinOp::And | BinOp::Or | BinOp::Lt | BinOp::Gt => {
                self.error(
                    format!("operator '{}' is not yet supported", op.symbol()),
                    span,
                );
                Type::Int
            }
            _ if lt == rt && is_numeric(&lt) => lt,
            _ => {
                self.error(
                    format!(
                        "cannot apply '{}' to {} and {}",
                        op.symbol(),
                        lt.name(),
                        rt.name()
                    ),
                    span,
                );
                lt
            }
        }
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Type {
        let name = match callee {
            Expr::Ident(n, _) => n.clone(),
            _ => {
                self.error("only named functions can be called".to_string(), span);
                return Type::Unit;
            }
        };
        // Copy the table reference out of `self` so the signature borrow is
        // independent of the `&mut self` calls below — no clone needed.
        let table = self.table;
        let sig = match table.functions.get(&name) {
            Some(sig) => sig,
            None => {
                self.error(format!("undefined function '{name}'"), span);
                return Type::Unit;
            }
        };
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
                        expected.name(),
                        got.name()
                    ),
                    arg.span(),
                );
            }
        }
        sig.ret.clone()
    }

    fn check_field(&mut self, base: &Expr, field: &str, span: Span) -> Type {
        let base_ty = self.type_of_expr(base);
        let struct_name = match &base_ty {
            Type::Struct(n) => n.clone(),
            _ => {
                self.error(format!("type {} has no fields", base_ty.name()), span);
                return Type::Unit;
            }
        };
        let field_ty = self
            .table
            .structs
            .get(&struct_name)
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
        let table = self.table;
        let decl = match table.structs.get(name) {
            Some(st) => st,
            None => {
                self.error(format!("unknown struct '{name}'"), span);
                for (_, value) in fields {
                    self.type_of_expr(value);
                }
                return Type::Unit;
            }
        };
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
                                expected.name(),
                                got.name()
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
        Type::Struct(name.to_string())
    }

    /// The innermost binding for `name`, searching scopes inside-out.
    fn find_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn lookup(&mut self, name: &str, span: Span) -> Type {
        if let Some(info) = self.find_var(name) {
            return info.ty.clone();
        }
        self.error(format!("undefined variable '{name}'"), span);
        Type::Unit
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::lex, parser::parse};

    fn table(src: &str) -> (SymbolTable, Vec<Diagnostic>) {
        let (tokens, ld) = lex(src);
        assert!(ld.is_empty(), "lex: {ld:?}");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse: {pd:?}");
        check(&ast)
    }

    #[test]
    fn collects_function_and_struct_signatures() {
        let (t, d) = table("struct P { x: int } fun f(a: int): float { return 1.0; }");
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
        assert_eq!(t.functions["f"].params, vec![Type::Int]);
        assert_eq!(t.functions["f"].ret, Type::Float);
        assert_eq!(t.structs["P"].fields, vec![("x".to_string(), Type::Int)]);
    }

    #[test]
    fn duplicate_function_is_an_error() {
        let (_, d) = table("fun f(): int { return 1; } fun f(): int { return 2; }");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("already defined"));
    }

    #[test]
    fn unknown_type_is_an_error() {
        let (_, d) = table("fun f(a: Missing): int { return 1; }");
        assert!(d.iter().any(|e| e.message.contains("unknown type 'Missing'")));
    }

    fn diags(src: &str) -> Vec<Diagnostic> {
        table(src).1
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
    fn unsupported_operator_is_reported() {
        let d = diags("fun f(a: int, b: int): int { const c = a < b; return a; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("'<' is not yet supported")),
            "{d:?}"
        );
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
    fn assigning_to_const_is_an_error() {
        let d = diags("fun f(): int { const x = 1; x = 2; return x; }");
        assert!(
            d.iter()
                .any(|e| e.message.contains("cannot assign to const 'x'")),
            "{d:?}"
        );
    }
}
