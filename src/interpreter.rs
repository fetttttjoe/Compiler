use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{BinOp, Expr, Function, Item, Stmt, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// Fields sorted by name (literals may write them in any order, and
    /// sorting makes `PartialEq` order-independent); lookup is a linear
    /// scan — structs are small, and the checker guarantees the field
    /// exists.
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
    },
    /// A `refstruct` instance: one shared object (always a `Value::Struct`
    /// inside), aliased by every copy of the handle.
    Ref(Rc<RefCell<Value>>),
    /// The `null` literal — the empty state of a `T?` slot.
    Null,
    Unit,
}

/// Like the derive, except `Ref` compares by identity (same object), not by
/// contents — so struct equality recursing into a ref field is identity too.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (
                Value::Struct { name: an, fields: af },
                Value::Struct { name: bn, fields: bf },
            ) => an == bn && af == bf,
            (Value::Ref(a), Value::Ref(b)) => Rc::ptr_eq(a, b),
            (Value::Null, Value::Null) => true,
            (Value::Unit, Value::Unit) => true,
            _ => false,
        }
    }
}

impl Value {
    /// Debug-style rendering with a depth cap — cyclic refstruct values
    /// (constructible since optionals landed) would recurse forever under
    /// derived `Debug`.
    // ponytail: depth cap, not cycle detection — 8 levels is plenty for a
    // result dump; switch to pointer-tracking if real output needs it.
    pub fn render(&self) -> String {
        self.render_depth(8)
    }

    fn render_depth(&self, depth: usize) -> String {
        if depth == 0 {
            return "...".to_string();
        }
        match self {
            Value::Ref(cell) => format!("Ref({})", cell.borrow().render_depth(depth - 1)),
            Value::Struct { name, fields } => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(f, v)| format!("{f}: {}", v.render_depth(depth - 1)))
                    .collect();
                format!("{name} {{ {} }}", fields.join(", "))
            }
            other => format!("{other:?}"),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "string",
            Value::Struct { .. } => "struct",
            Value::Ref(_) => "refstruct",
            Value::Null => "null",
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
            Stmt::Assign { target, value, .. } => {
                let v = self.eval(value)?;
                self.assign_place(target, v)?;
                Ok(Flow::Normal)
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
            Expr::Null(_) => Ok(Value::Null),
            Expr::Unary { op, rhs, span } => {
                let v = self.eval(rhs)?;
                eval_unary(*op, v, *span)
            }
            Expr::Binary { op, lhs, rhs, span } => match op {
                BinOp::And | BinOp::Or => self.eval_logical(*op, lhs, rhs),
                // `??` is lazy: the fallback runs only when the left is null.
                BinOp::Coalesce => match self.eval(lhs)? {
                    Value::Null => self.eval(rhs),
                    v => Ok(v),
                },
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
            // The checker has already verified literals are complete and
            // fields exist, so the error arms here are defensive only.
            Expr::StructLit { name, fields, .. } => {
                // Evaluate in written order (side effects), store sorted.
                let mut vals = Vec::with_capacity(fields.len());
                for (fname, fexpr) in fields {
                    vals.push((fname.clone(), self.eval(fexpr)?));
                }
                vals.sort_by(|a, b| a.0.cmp(&b.0));
                let value = Value::Struct {
                    name: name.clone(),
                    fields: vals,
                };
                // A refstruct literal allocates one shared object; everyone
                // who copies the handle aliases it.
                if self.resolutions.ref_structs[self.module].contains(name.as_str()) {
                    Ok(Value::Ref(Rc::new(RefCell::new(value))))
                } else {
                    Ok(value)
                }
            }
            Expr::Field {
                base,
                name,
                optional,
                span,
            } => match self.eval(base)? {
                // `?.` short-circuits on null; plain `.` never sees one
                // (the checker rejects it).
                Value::Null if *optional => Ok(Value::Null),
                v => get_field(&v, name, *span),
            },
        }
    }

    /// Writes `v` into the storage a place expression names. Field chains
    /// are read-modify-write: a value-struct hop clones the struct, sets the
    /// field in the copy, and writes the copy back into its own place; a
    /// refstruct hop mutates the shared object directly (that's the aliasing
    /// semantics — and it also ends the write-back, since the handle itself
    /// is unchanged). Error arms are defensive; the checker has already
    /// validated the place.
    // ponytail: value hops clone the intermediate structs — fine for the
    // interpreter (the oracle); codegen will write in place.
    fn assign_place(&mut self, target: &'a Expr, v: Value) -> Result<(), Diagnostic> {
        match target {
            Expr::Ident(name, span) => {
                match self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name)) {
                    Some(slot) => {
                        *slot = v;
                        Ok(())
                    }
                    None => Err(Diagnostic::error(
                        format!("undefined variable '{name}'"),
                        *span,
                    )),
                }
            }
            Expr::Field {
                base, name, span, ..
            } => match self.eval(base)? {
                // A refstruct hop mutates the shared object directly — the
                // aliasing semantics, and the end of the write-back chain.
                Value::Ref(cell) => set_field(&mut cell.borrow_mut(), name, v, *span),
                // A value hop: set the field in the copy, write the copy
                // back into its own place. (set_field rejects non-structs.)
                mut owned => {
                    set_field(&mut owned, name, v, *span)?;
                    self.assign_place(base, owned)
                }
            },
            _ => Err(Diagnostic::error(
                "invalid assignment target",
                target.span(),
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

/// Reads a field out of a struct value, following refstruct handles — the
/// read half of `Expr::Field`. Clones the field out (the oracle trades
/// copies for simplicity).
fn get_field(container: &Value, field: &str, span: Span) -> Result<Value, Diagnostic> {
    match container {
        Value::Struct { fields, .. } => fields
            .iter()
            .find(|(fname, _)| fname == field)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| Diagnostic::error(format!("no field '{field}'"), span)),
        Value::Ref(cell) => get_field(&cell.borrow(), field, span),
        other => Err(Diagnostic::error(
            format!("type {} has no fields", other.type_name()),
            span,
        )),
    }
}

/// Sets a field inside a (borrowed) struct value — the write half of a
/// field-chain hop in `assign_place`.
fn set_field(container: &mut Value, field: &str, v: Value, span: Span) -> Result<(), Diagnostic> {
    match container {
        Value::Struct { fields, .. } => {
            match fields.iter_mut().find(|(fname, _)| fname == field) {
                Some((_, slot)) => {
                    *slot = v;
                    Ok(())
                }
                None => Err(Diagnostic::error(format!("no field '{field}'"), span)),
            }
        }
        other => Err(Diagnostic::error(
            format!("type {} has no fields", other.type_name()),
            span,
        )),
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
        // Everything else — structs, refs, null — supports only equality:
        // structural for structs, identity for refs, presence for null, all
        // encoded in `Value::eq`. The checker guarantees the operands are
        // comparable; mixed pairs only reach here from rejected programs.
        (l, r) => match op {
            BinOp::Eq => Ok(Value::Bool(l == r)),
            BinOp::Ne => Ok(Value::Bool(l != r)),
            _ => Err(Diagnostic::error(
                format!(
                    "cannot apply '{}' to {} and {}",
                    op.symbol(),
                    l.type_name(),
                    r.type_name()
                ),
                span,
            )),
        },
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
        And | Or | Coalesce => unreachable!("short-circuiting operators are handled lazily in eval"),
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
        And | Or | Coalesce => unreachable!("short-circuiting operators are handled lazily in eval"),
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
    fn struct_literal_and_field_access() {
        let program = "\
struct Point { x: int, y: int }
fun main(): int {
    const p = Point { x: 3, y: 4 };
    return p.x * p.x + p.y * p.y;
}";
        assert_eq!(run(program), Ok(Value::Int(25)));
    }

    #[test]
    fn structs_pass_through_calls() {
        let program = "\
struct Point { x: int, y: int }
fun make(x: int, y: int): Point { return Point { x: x, y: y }; }
fun sum(p: Point): int { return p.x + p.y; }
fun main(): int { return sum(make(1, 2)); }";
        assert_eq!(run(program), Ok(Value::Int(3)));
    }

    #[test]
    fn nested_struct_field_access() {
        let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): int {
    const o = Outer { i: Inner { v: 7 } };
    return o.i.v;
}";
        assert_eq!(run(program), Ok(Value::Int(7)));
    }

    #[test]
    fn imported_struct_constructs_and_reads_across_modules() {
        assert_eq!(
            run_multi(&[
                (
                    "main.ys",
                    "import { Pair, make } from \"./lib\";\n\
                     fun main(): int { const p = make(); return p.a + Pair { a: 1, b: 2 }.b; }"
                ),
                (
                    "lib.ys",
                    "export struct Pair { a: int, b: int }\n\
                     export fun make(): Pair { return Pair { a: 40, b: 0 }; }"
                ),
            ]),
            Ok(Value::Int(42))
        );
    }

    #[test]
    fn struct_equality_compares_fields() {
        let program = "\
struct Point { x: int, y: int }
fun main(): bool {
    const a = Point { x: 1, y: 2 };
    const b = Point { x: 1, y: 2 };
    const c = Point { x: 9, y: 2 };
    return a == b && a != c;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn struct_equality_ignores_literal_field_order() {
        let program = "\
struct Point { x: int, y: int }
fun main(): bool {
    return Point { x: 1, y: 2 } == Point { y: 2, x: 1 };
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn nested_struct_equality_recurses() {
        let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): bool {
    const a = Outer { i: Inner { v: 1 } };
    const b = Outer { i: Inner { v: 2 } };
    return a != b;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn field_assignment_mutates_the_struct() {
        let program = "\
struct Point { x: int, y: int }
fun main(): int {
    var p = Point { x: 1, y: 2 };
    p.x = 40;
    return p.x + p.y;
}";
        assert_eq!(run(program), Ok(Value::Int(42)));
    }

    #[test]
    fn nested_field_assignment() {
        let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): int {
    var o = Outer { i: Inner { v: 1 } };
    o.i.v = 9;
    return o.i.v;
}";
        assert_eq!(run(program), Ok(Value::Int(9)));
    }

    #[test]
    fn refstruct_aliases_share_mutation() {
        let program = "\
refstruct P { x: int }
fun main(): int {
    const a = P { x: 1 };
    const b = a;
    b.x = 5;
    return a.x;
}";
        assert_eq!(run(program), Ok(Value::Int(5)));
    }

    #[test]
    fn functions_mutate_refstruct_arguments() {
        let program = "\
refstruct P { x: int }
fun bump(p: P) { p.x = p.x + 1; }
fun main(): int {
    const p = P { x: 1 };
    bump(p);
    bump(p);
    return p.x;
}";
        assert_eq!(run(program), Ok(Value::Int(3)));
    }

    #[test]
    fn refstruct_equality_is_identity() {
        let program = "\
refstruct P { x: int }
fun main(): bool {
    const a = P { x: 1 };
    const b = P { x: 1 };
    const c = a;
    return a == c && a != b;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn value_struct_copies_stay_independent() {
        // Pins ADR 0005: plain structs copy; mutating the copy leaves the
        // original untouched.
        let program = "\
struct V { x: int }
fun main(): int {
    var a = V { x: 1 };
    var b = a;
    b.x = 9;
    return a.x;
}";
        assert_eq!(run(program), Ok(Value::Int(1)));
    }

    #[test]
    fn value_struct_compares_ref_fields_by_identity() {
        let program = "\
refstruct R { v: int }
struct Box { r: R }
fun main(): bool {
    const r1 = R { v: 1 };
    const r2 = R { v: 1 };
    const a = Box { r: r1 };
    const b = Box { r: r1 };
    const c = Box { r: r2 };
    return a == b && a != c;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn mutation_through_a_mixed_value_ref_chain() {
        let program = "\
refstruct R { v: int }
struct Box { r: R }
fun main(): int {
    const b = Box { r: R { v: 1 } };
    b.r.v = 7;
    return b.r.v;
}";
        assert_eq!(run(program), Ok(Value::Int(7)));
    }

    #[test]
    fn imported_refstruct_mutates_across_modules() {
        assert_eq!(
            run_multi(&[
                (
                    "main.ys",
                    "import { Counter, bump } from \"./lib\";\n\
                     fun main(): int { const c = Counter { n: 0 }; bump(c); bump(c); return c.n; }"
                ),
                (
                    "lib.ys",
                    "export refstruct Counter { n: int }\n\
                     export fun bump(c: Counter) { c.n = c.n + 1; }"
                ),
            ]),
            Ok(Value::Int(2))
        );
    }

    #[test]
    fn optional_chaining_short_circuits_on_null() {
        let program = "\
refstruct P { x: int }
fun get(p: P?): int { return p?.x ?? 42; }
fun main(): int { return get(null) + get(P { x: 1 }); }";
        assert_eq!(run(program), Ok(Value::Int(43)));
    }

    #[test]
    fn null_equality_at_runtime() {
        let program = "\
refstruct P { x: int }
fun main(): bool {
    var p: P? = null;
    const was_null = p == null;
    p = P { x: 1 };
    return was_null && p != null;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn linked_list_builds_traverses_and_mutates() {
        let program = "\
refstruct Node { v: int, next: Node? }
fun main(): int {
    const head = Node { v: 1, next: Node { v: 2, next: Node { v: 3, next: null } } };
    var cur: Node? = head;
    var sum = 0;
    while cur != null {
        cur.v = cur.v * 10;
        sum = sum + cur.v;
        cur = cur.next;
    }
    return sum;
}";
        assert_eq!(run(program), Ok(Value::Int(60)));
    }

    #[test]
    fn cyclic_values_render_finitely() {
        let program = "\
refstruct Node { v: int, next: Node? }
fun main(): Node {
    const a = Node { v: 1, next: null };
    a.next = a;
    return a;
}";
        let rendered = run(program).unwrap().render();
        assert!(rendered.contains("Node") && rendered.contains("..."), "{rendered}");
        assert!(rendered.len() < 500, "unbounded: {} bytes", rendered.len());
    }

    #[test]
    fn scalar_rendering_matches_debug() {
        assert_eq!(Value::Int(55).render(), "Int(55)");
        assert_eq!(Value::Bool(true).render(), "Bool(true)");
    }

    /// The kitchen-sink program: a binary search tree combining refstruct
    /// mutation through const/param bindings, optional args and returns,
    /// else-branch narrowing, recursion, `??` laziness, and ref identity.
    #[test]
    fn binary_search_tree_end_to_end() {
        let program = "\
refstruct Tree { v: int, left: Tree?, right: Tree? }
fun insert(t: Tree?, v: int): Tree {
    if t == null {
        return Tree { v: v, left: null, right: null };
    } else {
        if v < t.v { t.left = insert(t.left, v); } else { t.right = insert(t.right, v); }
        return t;
    }
}
fun sum(t: Tree?): int {
    if t == null { return 0; } else { return sum(t.left) + t.v + sum(t.right); }
}
fun min(t: Tree): int {
    var best = t.v;
    var cur: Tree? = t.left;
    while cur != null { best = cur.v; cur = cur.left; }
    return best;
}
fun main(): bool {
    var root: Tree? = null;
    root = insert(root, 5);
    const keep = root;
    root = insert(root, 3);
    root = insert(root, 8);
    root = insert(root, 1);
    return sum(root) == 17 && keep == root && min(keep ?? insert(null, 0)) == 1;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn coalesce_chains_left_associatively() {
        let program = "\
fun main(): int {
    var a: int? = null;
    var b: int? = null;
    return (a ?? b ?? 7) + (a ?? 1);
}";
        assert_eq!(run(program), Ok(Value::Int(8)));
    }

    #[test]
    fn returned_refstruct_keeps_identity() {
        let program = "\
refstruct P { x: int }
fun same(p: P): P { return p; }
fun main(): bool {
    const a = P { x: 1 };
    return same(a) == a;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
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
