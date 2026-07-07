use std::collections::HashMap;

use crate::ast::{BinOp, Expr, Function, Item, Stmt, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Unit,
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "string",
            Value::Unit => "unit",
        }
    }
}

/// Runs `main()` from the entry module (graph index 0), resolving every call
/// through its module's alias map. Returns `Unit` when there is no `main`.
pub fn interpret(graph: &ModuleGraph, resolutions: &Resolutions) -> Result<Value, Diagnostic> {
    let mut functions: HashMap<(usize, &str), &Function> = HashMap::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                functions.insert((mi, f.name.as_str()), f);
            }
        }
    }
    let mut interp = Interp {
        functions,
        resolutions,
        module: 0,
        scopes: Vec::new(),
    };
    match interp.functions.get(&(0, "main")).copied() {
        Some(main) => interp.call(main, 0, Vec::new(), Span::new(0, 0)),
        None => Ok(Value::Unit),
    }
}

enum Flow {
    Normal,
    Return(Value),
}

struct Interp<'a> {
    functions: HashMap<(usize, &'a str), &'a Function>,
    resolutions: &'a Resolutions,
    /// The module whose alias map resolves calls in the currently executing
    /// function — saved/restored around every call.
    module: usize,
    scopes: Vec<HashMap<String, Value>>,
}

impl<'a> Interp<'a> {
    fn call(
        &mut self,
        func: &'a Function,
        module: usize,
        args: Vec<Value>,
        _span: Span,
    ) -> Result<Value, Diagnostic> {
        let mut scope = HashMap::new();
        for (param, value) in func.params.iter().zip(args) {
            scope.insert(param.name.clone(), value);
        }
        let prev_module = self.module;
        self.module = module;
        self.scopes.push(scope);
        let result = self.exec_block(&func.body);
        self.scopes.pop();
        self.module = prev_module;
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
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                if self.eval_condition(cond)? {
                    self.exec_block_scoped(then_body)
                } else if let Some(else_body) = else_body {
                    self.exec_block_scoped(else_body)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While { cond, body, .. } => {
                while self.eval_condition(cond)? {
                    if let Flow::Return(v) = self.exec_block_scoped(body)? {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Assign { name, value, span } => {
                let v = self.eval(value)?;
                match self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name)) {
                    Some(slot) => {
                        *slot = v;
                        Ok(Flow::Normal)
                    }
                    None => Err(Diagnostic::error(
                        format!("undefined variable '{name}'"),
                        *span,
                    )),
                }
            }
        }
    }

    /// Runs a nested block in its own scope: bindings made inside die at the
    /// closing brace (mirrors the checker's scoping).
    fn exec_block_scoped(&mut self, body: &'a [Stmt]) -> Result<Flow, Diagnostic> {
        self.scopes.push(HashMap::new());
        let flow = self.exec_block(body);
        self.scopes.pop();
        flow
    }

    fn eval_condition(&mut self, cond: &'a Expr) -> Result<bool, Diagnostic> {
        match self.eval(cond)? {
            Value::Bool(b) => Ok(b),
            other => Err(Diagnostic::error(
                format!("condition must be bool, found {}", other.type_name()),
                cond.span(),
            )),
        }
    }

    /// `&&` / `||` evaluate lazily: the right side runs only when the left
    /// side hasn't already decided the result.
    fn eval_logical(
        &mut self,
        op: BinOp,
        lhs: &'a Expr,
        rhs: &'a Expr,
    ) -> Result<Value, Diagnostic> {
        let l = self.eval_bool_operand(lhs, op)?;
        match (op, l) {
            (BinOp::And, false) => Ok(Value::Bool(false)),
            (BinOp::Or, true) => Ok(Value::Bool(true)),
            _ => Ok(Value::Bool(self.eval_bool_operand(rhs, op)?)),
        }
    }

    fn eval_bool_operand(&mut self, expr: &'a Expr, op: BinOp) -> Result<bool, Diagnostic> {
        match self.eval(expr)? {
            Value::Bool(b) => Ok(b),
            other => Err(Diagnostic::error(
                format!("cannot apply '{}' to {}", op.symbol(), other.type_name()),
                expr.span(),
            )),
        }
    }

    fn eval(&mut self, expr: &'a Expr) -> Result<Value, Diagnostic> {
        match expr {
            Expr::Int(n, _) => Ok(Value::Int(*n)),
            Expr::Float(f, _) => Ok(Value::Float(*f)),
            Expr::Bool(b, _) => Ok(Value::Bool(*b)),
            Expr::Str(s, _) => Ok(Value::Str(s.clone())),
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Unary { op, rhs, span } => {
                let v = self.eval(rhs)?;
                eval_unary(*op, v, *span)
            }
            Expr::Binary { op, lhs, rhs, span } => match op {
                BinOp::And | BinOp::Or => self.eval_logical(*op, lhs, rhs),
                _ => {
                    let l = self.eval(lhs)?;
                    let r = self.eval(rhs)?;
                    eval_binary(*op, l, r, *span)
                }
            },
            Expr::Call { callee, args, span } => {
                let name = match callee.as_ref() {
                    Expr::Ident(n, _) => n.clone(),
                    _ => {
                        return Err(Diagnostic::error(
                            "only named functions can be called",
                            *span,
                        ))
                    }
                };
                let Some((target_module, target_name)) =
                    self.resolutions.functions[self.module].get(name.as_str()).cloned()
                else {
                    return Err(Diagnostic::error(
                        format!("undefined function '{name}'"),
                        *span,
                    ));
                };
                let func = match self
                    .functions
                    .get(&(target_module, target_name.as_str()))
                    .copied()
                {
                    Some(f) => f,
                    None => {
                        return Err(Diagnostic::error(
                            format!("undefined function '{name}'"),
                            *span,
                        ))
                    }
                };
                let mut argv = Vec::with_capacity(args.len());
                for arg in args {
                    argv.push(self.eval(arg)?);
                }
                self.call(func, target_module, argv, *span)
            }
            // Struct runtime values are deferred (they still parse and type-check).
            Expr::Field { span, .. } | Expr::StructLit { span, .. } => Err(Diagnostic::error(
                "struct values are not yet supported at runtime",
                *span,
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
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnOp::Neg, other) => Err(Diagnostic::error(
            format!("cannot negate {}", other.type_name()),
            span,
        )),
        (UnOp::Not, other) => Err(Diagnostic::error(
            format!("cannot apply '!' to {}", other.type_name()),
            span,
        )),
    }
}

/// Strict binary evaluation; `&&`/`||` never reach here (they short-circuit
/// in `eval_logical`).
fn eval_binary(op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Diagnostic> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => int_op(op, a, b, span),
        (Value::Float(a), Value::Float(b)) => float_op(op, a, b, span),
        (Value::Str(a), Value::Str(b)) => str_op(op, a, b, span),
        (Value::Bool(a), Value::Bool(b)) => bool_op(op, a, b, span),
        (a, b) => Err(Diagnostic::error(
            format!(
                "cannot apply '{}' to {} and {}",
                op.symbol(),
                a.type_name(),
                b.type_name()
            ),
            span,
        )),
    }
}

fn int_op(op: BinOp, a: i64, b: i64, span: Span) -> Result<Value, Diagnostic> {
    use BinOp::*;
    let v = match op {
        Add => Value::Int(a.wrapping_add(b)),
        Sub => Value::Int(a.wrapping_sub(b)),
        Mul => Value::Int(a.wrapping_mul(b)),
        Div | Rem if b == 0 => return Err(Diagnostic::error("division by zero", span)),
        Div => Value::Int(a / b),
        Rem => Value::Int(a % b),
        Eq => Value::Bool(a == b),
        Ne => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Le => Value::Bool(a <= b),
        Gt => Value::Bool(a > b),
        Ge => Value::Bool(a >= b),
        And | Or => unreachable!("logical operators short-circuit in eval_logical"),
    };
    Ok(v)
}

// `_span` kept for signature symmetry with `int_op`; float division by zero
// follows IEEE (infinity/NaN), so floats have no erroring operations.
fn float_op(op: BinOp, a: f64, b: f64, _span: Span) -> Result<Value, Diagnostic> {
    use BinOp::*;
    let v = match op {
        // Division by zero follows IEEE (infinity/NaN), so no error arm here.
        Add => Value::Float(a + b),
        Sub => Value::Float(a - b),
        Mul => Value::Float(a * b),
        Div => Value::Float(a / b),
        Rem => Value::Float(a % b),
        Eq => Value::Bool(a == b),
        Ne => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Le => Value::Bool(a <= b),
        Gt => Value::Bool(a > b),
        Ge => Value::Bool(a >= b),
        And | Or => unreachable!("logical operators short-circuit in eval_logical"),
    };
    Ok(v)
}

fn str_op(op: BinOp, a: String, b: String, span: Span) -> Result<Value, Diagnostic> {
    match op {
        BinOp::Add => Ok(Value::Str(a + &b)),
        BinOp::Eq => Ok(Value::Bool(a == b)),
        BinOp::Ne => Ok(Value::Bool(a != b)),
        _ => Err(Diagnostic::error(
            format!("cannot apply '{}' to string and string", op.symbol()),
            span,
        )),
    }
}

fn bool_op(op: BinOp, a: bool, b: bool, span: Span) -> Result<Value, Diagnostic> {
    match op {
        BinOp::Eq => Ok(Value::Bool(a == b)),
        BinOp::Ne => Ok(Value::Bool(a != b)),
        _ => Err(Diagnostic::error(
            format!("cannot apply '{}' to bool and bool", op.symbol()),
            span,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{load_program, Module};
    use crate::source::SourceMap;
    use crate::{check::check, lexer::lex, parser::parse};

    fn run(src: &str) -> Result<Value, Diagnostic> {
        let (tokens, ld) = lex(src);
        assert!(ld.is_empty(), "lex: {ld:?}");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse: {pd:?}");
        let graph = ModuleGraph {
            modules: vec![Module {
                path: "test.ys".to_string(),
                ast,
                imports: Vec::new(),
            }],
        };
        let (res, cd) = check(&graph);
        assert!(cd.is_empty(), "check: {cd:?}");
        interpret(&graph, &res)
    }

    /// Full pipeline over in-memory files; the first file is the entry.
    fn run_multi(files: &[(&str, &str)]) -> Result<Value, Diagnostic> {
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
        let (res, cd) = check(&graph);
        assert!(cd.is_empty(), "check: {cd:?}");
        interpret(&graph, &res)
    }

    #[test]
    fn cross_module_calls_execute() {
        assert_eq!(
            run_multi(&[
                (
                    "main.ys",
                    "import { double } from \"./lib\";\nfun main(): int { return double(21); }"
                ),
                ("lib.ys", "export fun double(n: int): int { return n * 2; }"),
            ]),
            Ok(Value::Int(42))
        );
    }

    #[test]
    fn same_named_functions_resolve_within_their_own_module() {
        // Both modules define `helper`; each function must call its own.
        assert_eq!(
            run_multi(&[
                (
                    "main.ys",
                    "import { a } from \"./a\";\n\
                     fun helper(): int { return 2; }\n\
                     fun main(): int { return a() + helper(); }"
                ),
                (
                    "a.ys",
                    "fun helper(): int { return 1; }\n\
                     export fun a(): int { return helper(); }"
                ),
            ]),
            Ok(Value::Int(3))
        );
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

    #[test]
    fn end_to_end_function_calls() {
        let program = "\
fun square(n: int): int { return n * n; }
fun main(): int {
    const a = square(3);
    var b = 4;
    b = b + a;
    return b;
}";
        assert_eq!(run(program), Ok(Value::Int(13)));
    }

    #[test]
    fn nested_calls() {
        let program = "\
fun inc(n: int): int { return n + 1; }
fun main(): int { return inc(inc(inc(0))); }";
        assert_eq!(run(program), Ok(Value::Int(3)));
    }

    #[test]
    fn comparisons_and_equality_evaluate() {
        assert_eq!(
            run("fun main(): bool { return 1 + 2 == 3; }"),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            run("fun main(): bool { return 2.0 <= 1.5; }"),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            run("fun main(): bool { return \"a\" != \"b\"; }"),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn logical_operators_short_circuit() {
        // The right side would divide by zero — short-circuiting must skip it.
        assert_eq!(
            run("fun main(): bool { return false && 1 / 0 == 0; }"),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            run("fun main(): bool { return true || 1 / 0 == 0; }"),
            Ok(Value::Bool(true))
        );
        // And without short-circuit conditions, both sides evaluate.
        assert_eq!(
            run("fun main(): bool { return true && !false; }"),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(
            run("fun main(): string { return \"foo\" + \"bar\"; }"),
            Ok(Value::Str("foobar".to_string()))
        );
    }

    #[test]
    fn if_else_selects_the_right_branch() {
        let abs = "\
fun abs(n: int): int {
    if n < 0 { return -n; } else { return n; }
}
fun main(): int { return abs(-7) + abs(7); }";
        assert_eq!(run(abs), Ok(Value::Int(14)));
    }

    #[test]
    fn while_loop_accumulates() {
        let program = "\
fun main(): int {
    var i = 0;
    var acc = 0;
    while i < 5 {
        i = i + 1;
        acc = acc + i;
    }
    return acc;
}";
        assert_eq!(run(program), Ok(Value::Int(15)));
    }

    #[test]
    fn block_scopes_shadow_and_expire() {
        // The inner `const x` shadows the outer `var x` inside the block only.
        let program = "\
fun main(): int {
    var x = 1;
    if true { const x = 10; }
    return x;
}";
        assert_eq!(run(program), Ok(Value::Int(1)));
    }

    #[test]
    fn recursion_with_control_flow() {
        let fib = "\
fun fib(n: int): int {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fun main(): int { return fib(10); }";
        assert_eq!(run(fib), Ok(Value::Int(55)));
    }
}
