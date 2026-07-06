use std::collections::HashMap;

use crate::ast::{Ast, BinOp, Expr, Function, Item, Stmt, UnOp};
use crate::diagnostic::Diagnostic;
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Unit,
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Unit => "unit",
        }
    }
}

/// Runs the program's `main()` (no arguments). Returns `Unit` if there is none.
pub fn interpret(ast: &Ast) -> Result<Value, Diagnostic> {
    let mut functions = HashMap::new();
    for item in ast {
        if let Item::Function(f) = item {
            functions.insert(f.name.clone(), f);
        }
    }
    let mut interp = Interp {
        functions,
        scopes: Vec::new(),
    };
    match interp.functions.get("main").copied() {
        Some(main) => interp.call(main, Vec::new(), Span::new(0, 0)),
        None => Ok(Value::Unit),
    }
}

enum Flow {
    Normal,
    Return(Value),
}

struct Interp<'a> {
    functions: HashMap<String, &'a Function>,
    scopes: Vec<HashMap<String, Value>>,
}

impl<'a> Interp<'a> {
    fn call(
        &mut self,
        func: &'a Function,
        args: Vec<Value>,
        _span: Span,
    ) -> Result<Value, Diagnostic> {
        let mut scope = HashMap::new();
        for (param, value) in func.params.iter().zip(args) {
            scope.insert(param.name.clone(), value);
        }
        self.scopes.push(scope);
        let result = self.exec_block(&func.body);
        self.scopes.pop();
        Ok(match result? {
            Flow::Return(v) => v,
            Flow::Normal => Value::Unit,
        })
    }

    fn exec_block(&mut self, body: &'a [Stmt]) -> Result<Flow, Diagnostic> {
        for stmt in body {
            if let Flow::Return(v) = self.exec_stmt(stmt)? {
                return Ok(Flow::Return(v));
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &'a Stmt) -> Result<Flow, Diagnostic> {
        match stmt {
            Stmt::Let { name, value, .. } => {
                let v = self.eval(value)?;
                self.scopes.last_mut().unwrap().insert(name.clone(), v);
                Ok(Flow::Normal)
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(v))
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Assign { .. } => Ok(Flow::Normal), // implemented in Task 13
        }
    }

    fn eval(&mut self, expr: &'a Expr) -> Result<Value, Diagnostic> {
        match expr {
            Expr::Int(n, _) => Ok(Value::Int(*n)),
            Expr::Float(f, _) => Ok(Value::Float(*f)),
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Unary { op, rhs, span } => {
                let v = self.eval(rhs)?;
                eval_unary(*op, v, *span)
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                eval_binary(*op, l, r, *span)
            }
            // Call added in Task 13.
            _ => Err(Diagnostic::error(
                "evaluation not yet supported",
                expr.span(),
            )),
        }
    }

    fn lookup(&self, name: &str, span: Span) -> Result<Value, Diagnostic> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Ok(v.clone());
            }
        }
        Err(Diagnostic::error(
            format!("undefined variable '{name}'"),
            span,
        ))
    }
}

fn eval_unary(op: UnOp, v: Value, span: Span) -> Result<Value, Diagnostic> {
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
        (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnOp::Not, _) => Err(Diagnostic::error(
            "operator '!' is not yet supported",
            span,
        )),
        (UnOp::Neg, other) => Err(Diagnostic::error(
            format!("cannot negate {}", other.type_name()),
            span,
        )),
    }
}

fn eval_binary(op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Diagnostic> {
    match op {
        BinOp::And | BinOp::Or | BinOp::Lt | BinOp::Gt => Err(Diagnostic::error(
            format!("operator '{}' is not yet supported", op.symbol()),
            span,
        )),
        _ => match (l, r) {
            (Value::Int(a), Value::Int(b)) => int_arith(op, a, b, span),
            (Value::Float(a), Value::Float(b)) => float_arith(op, a, b, span),
            (a, b) => Err(Diagnostic::error(
                format!(
                    "cannot apply '{}' to {} and {}",
                    op.symbol(),
                    a.type_name(),
                    b.type_name()
                ),
                span,
            )),
        },
    }
}

fn int_arith(op: BinOp, a: i64, b: i64, span: Span) -> Result<Value, Diagnostic> {
    let v = match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(Diagnostic::error("division by zero", span));
            }
            a / b
        }
        BinOp::Rem => {
            if b == 0 {
                return Err(Diagnostic::error("division by zero", span));
            }
            a % b
        }
        _ => unreachable!("non-arithmetic operator reached int_arith"),
    };
    Ok(Value::Int(v))
}

fn float_arith(op: BinOp, a: f64, b: f64, span: Span) -> Result<Value, Diagnostic> {
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::Rem => a % b,
        _ => unreachable!("non-arithmetic operator reached float_arith"),
    };
    Ok(Value::Float(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{check::check, lexer::lex, parser::parse};

    fn run(src: &str) -> Result<Value, Diagnostic> {
        let (tokens, ld) = lex(src);
        assert!(ld.is_empty(), "lex: {ld:?}");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse: {pd:?}");
        let (_t, cd) = check(&ast);
        assert!(cd.is_empty(), "check: {cd:?}");
        interpret(&ast)
    }

    #[test]
    fn arithmetic_respects_precedence() {
        assert_eq!(
            run("fun main(): int { return 1 + 2 * 3; }"),
            Ok(Value::Int(7))
        );
    }

    #[test]
    fn local_bindings_and_unary() {
        assert_eq!(
            run("fun main(): int { const x = 10; return -x + 2; }"),
            Ok(Value::Int(-8))
        );
    }

    #[test]
    fn float_arithmetic() {
        assert_eq!(
            run("fun main(): float { return 1.5 * 2.0; }"),
            Ok(Value::Float(3.0))
        );
    }

    #[test]
    fn no_main_returns_unit() {
        assert_eq!(run("fun other(): int { return 1; }"), Ok(Value::Unit));
    }

    #[test]
    fn division_by_zero_is_a_runtime_error() {
        assert!(run("fun main(): int { return 10 / 0; }").is_err());
    }
}
