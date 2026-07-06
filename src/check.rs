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
            Stmt::Assign { .. } => {
                // Added in Task 11.
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
            // Call / Field / StructLit added in Task 11.
            _ => {
                self.error("expression is not yet supported".to_string(), expr.span());
                Type::Unit
            }
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

    fn lookup(&mut self, name: &str, span: Span) -> Type {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.get(name) {
                return info.ty.clone();
            }
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
}
