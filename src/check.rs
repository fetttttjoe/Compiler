use std::collections::{HashMap, HashSet};

use crate::ast::{Ast, Item, TypeAnn};
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
    // Body checking is added in Tasks 10 and 11.
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
}
