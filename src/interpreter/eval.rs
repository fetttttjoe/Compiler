//! The tree-walker: one `Interp` per program run, evaluating statements
//! and expressions directly over the AST. Runs on its own 1GB thread
//! with an explicit depth budget (ADR 0011) so deep programs become
//! diagnostics, never stack overflows. The value-operation helpers at
//! the bottom define the language's arithmetic — wrapping ints, IEEE
//! floats, content-equal strings.

use super::*;

pub(super) fn run_program(
    graph: &ModuleGraph,
    resolutions: &Resolutions,
) -> Result<(Value, Heap), Diagnostic> {
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
        depth: 0,
        heap: Heap::default(),
    };
    let value = match interp.functions.get(&(0, syntax::ENTRY_FN)).copied() {
        Some(main) => interp.call(main, 0, Vec::new(), Span::new(0, 0))?,
        None => Value::Unit,
    };
    Ok((value, interp.heap))
}

enum Flow {
    Normal,
    Return(Value),
    /// `break` / `continue` unwinding toward the innermost loop, which
    /// consumes it; the checker proves one exists (ADR 0019).
    Break,
    Continue,
}

struct Interp<'a> {
    functions: HashMap<(usize, &'a str), &'a Function>,
    resolutions: &'a Resolutions,
    /// The module whose alias map resolves calls in the currently executing
    /// function — saved/restored around every call.
    module: usize,
    scopes: Vec<HashMap<String, Value>>,
    /// Current language-call depth — bounded as language policy in `call`.
    depth: usize,
    heap: Heap,
}

impl<'a> Interp<'a> {
    fn call(
        &mut self,
        func: &'a Function,
        module: usize,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, Diagnostic> {
        // Calls, statements, and expressions all charge the one depth
        // budget (see the policy block up top) — the diagnostic here just
        // gets to name the function.
        if self.depth >= MAX_EVAL_DEPTH {
            return Err(Diagnostic::error(
                format!("evaluation depth limit exceeded in '{}'", func.name),
                span,
            ));
        }
        self.depth += 1;
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
        self.depth -= 1;
        Ok(match result? {
            Flow::Return(v) => v,
            Flow::Normal => Value::Unit,
            Flow::Break | Flow::Continue => {
                unreachable!("checker rejects break/continue outside loops")
            }
        })
    }

    fn exec_block(&mut self, body: &'a [Stmt]) -> Result<Flow, Diagnostic> {
        for stmt in body {
            match self.exec_stmt(stmt)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &'a Stmt) -> Result<Flow, Diagnostic> {
        self.charge(stmt.span())?;
        let flow = self.exec_stmt_inner(stmt);
        self.depth -= 1;
        flow
    }

    fn exec_stmt_inner(&mut self, stmt: &'a Stmt) -> Result<Flow, Diagnostic> {
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
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
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
                    match self.exec_block_scoped(body)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For {
                index,
                name,
                iterable,
                body,
                span,
            } => {
                let id = match self.eval(iterable)? {
                    Value::Array(id) => id,
                    other => {
                        return Err(Diagnostic::error(
                            format!("can only iterate over arrays, found {}", other.type_name()),
                            *span,
                        ));
                    }
                };
                // Live iteration: the body may push or mutate, so length
                // and elements are re-read from the heap each step (element
                // cloned out before the body runs).
                let mut i = 0;
                loop {
                    let item = self.heap.arrays[id].get(i).cloned();
                    let Some(item) = item else { break };
                    let mut scope = HashMap::from([(name.clone(), item)]);
                    if let Some(index) = index {
                        scope.insert(index.clone(), Value::Int(i as i64));
                    }
                    self.scopes.push(scope);
                    let flow = self.exec_block(body);
                    self.scopes.pop();
                    match flow? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        // `continue` still advances — the increment is here.
                        Flow::Continue | Flow::Normal => {}
                    }
                    i += 1;
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
        self.charge(expr.span())?;
        let value = self.eval_inner(expr);
        self.depth -= 1;
        value
    }

    /// Statements and expressions recurse on the native stack, so every
    /// level draws one unit from the shared depth budget.
    fn charge(&mut self, span: Span) -> Result<(), Diagnostic> {
        if self.depth >= MAX_EVAL_DEPTH {
            return Err(Diagnostic::error(
                "evaluation depth limit exceeded (recursion or nesting too deep)".to_string(),
                span,
            ));
        }
        self.depth += 1;
        Ok(())
    }

    fn eval_inner(&mut self, expr: &'a Expr) -> Result<Value, Diagnostic> {
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
            // ADR 0028: float(i) is total (nearest-even); int(f)
            // truncates toward zero and is checked - valid iff
            // f in [-2^63, 2^63), which NaN fails by comparing false.
            // ADR 0029: string(x) is print's text for any value.
            Expr::Convert { to, arg, span, .. } => match (self.eval(arg)?, *to) {
                (Value::Int(i), Conv::Float) => Ok(Value::Float(i as f64)),
                (Value::Float(f), Conv::Int) => {
                    if (-9223372036854775808.0..9223372036854775808.0).contains(&f) {
                        Ok(Value::Int(f as i64))
                    } else {
                        Err(Diagnostic::error("invalid float to int conversion", *span))
                    }
                }
                (v, Conv::Str) => Ok(Value::Str(v.display(&self.heap))),
                _ => unreachable!("checker enforced the operand type"),
            },
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
                        ));
                    }
                };
                let Some((target_module, target_name)) = self.resolutions.functions[self.module]
                    .get(name.as_str())
                    .cloned()
                else {
                    // Builtins run only when no user definition shadows them
                    // — mirrors the checker's resolution order. Shape errors
                    // are defensive; the checker validated arities and types.
                    if name == syntax::BUILTIN_PRINT && args.len() == 1 {
                        let v = self.eval(&args[0])?;
                        use std::io::Write;
                        if let Err(e) = writeln!(std::io::stdout(), "{}", v.display(&self.heap)) {
                            // A closed pipe means the consumer is done —
                            // stop quietly (GNU convention). Anything else
                            // (full disk, bad fd) is a real error.
                            if e.kind() == std::io::ErrorKind::BrokenPipe {
                                std::process::exit(0);
                            }
                            return Err(Diagnostic::error(
                                format!("cannot write output: {e}"),
                                *span,
                            ));
                        }
                        return Ok(Value::Unit);
                    }
                    if name == syntax::BUILTIN_LEN && args.len() == 1 {
                        return match self.eval(&args[0])? {
                            Value::Array(id) => Ok(Value::Int(self.heap.arrays[id].len() as i64)),
                            other => Err(Diagnostic::error(
                                format!("'len' expects an array, found {}", other.type_name()),
                                *span,
                            )),
                        };
                    }
                    if name == syntax::BUILTIN_PUSH && args.len() == 2 {
                        let array = self.eval(&args[0])?;
                        let value = self.eval(&args[1])?;
                        return match array {
                            Value::Array(id) => {
                                self.heap.arrays[id].push(value);
                                Ok(Value::Unit)
                            }
                            other => Err(Diagnostic::error(
                                format!("'push' expects an array, found {}", other.type_name()),
                                *span,
                            )),
                        };
                    }
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
                        ));
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
            Expr::StructLit { name, fields, span } => {
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
                // A refstruct literal allocates one shared heap object;
                // everyone who copies the handle aliases it.
                if self.resolutions.ref_structs[self.module].contains(name.as_str()) {
                    let Value::Struct { name, fields } = value else {
                        unreachable!("struct literals evaluate to structs")
                    };
                    self.check_heap(*span)?;
                    Ok(Value::Ref({
                        self.heap.structs.push(StructObj { name, fields });
                        self.heap.structs.len() - 1
                    }))
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
                v => self.get_field(&v, name, *span),
            },
            Expr::ArrayLit { elements, span } => {
                let mut items = Vec::with_capacity(elements.len());
                for element in elements {
                    items.push(self.eval(element)?);
                }
                self.check_heap(*span)?;
                self.heap.arrays.push(items);
                Ok(Value::Array(self.heap.arrays.len() - 1))
            }
            Expr::Index { base, index, span } => {
                let array = self.eval(base)?;
                let index = self.eval(index)?;
                let (id, i) = self.index_array(array, index, *span)?;
                Ok(self.heap.arrays[id][i].clone())
            }
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
                *self.slot_mut(name, *span)? = v;
                Ok(())
            }
            Expr::Field {
                base, name, span, ..
            } => match self.eval(base)? {
                // A refstruct hop mutates the shared heap object directly —
                // the aliasing semantics, and the end of the write-back
                // chain (the handle itself is unchanged).
                Value::Ref(id) => set_in_fields(&mut self.heap.structs[id].fields, name, v, *span),
                // A value hop: set the field in the copy, write the copy
                // back into its own place.
                Value::Struct {
                    name: struct_name,
                    mut fields,
                } => {
                    set_in_fields(&mut fields, name, v, *span)?;
                    self.assign_place(
                        base,
                        Value::Struct {
                            name: struct_name,
                            fields,
                        },
                    )
                }
                other => Err(Diagnostic::error(
                    format!("type {} has no fields", other.type_name()),
                    *span,
                )),
            },
            Expr::Index { base, index, span } => {
                let array = self.eval(base)?;
                let index = self.eval(index)?;
                let (id, i) = self.index_array(array, index, *span)?;
                self.heap.arrays[id][i] = v;
                Ok(())
            }
            _ => Err(Diagnostic::error(
                "invalid assignment target",
                target.span(),
            )),
        }
    }

    /// The arena never frees, so runaway allocation must become a
    /// sanctioned diagnostic (like the depth limit) instead of an OOM kill.
    fn check_heap(&self, span: Span) -> Result<(), Diagnostic> {
        if self.heap.cell_count() >= MAX_HEAP_CELLS {
            return Err(Diagnostic::error(
                format!("heap limit ({MAX_HEAP_CELLS} objects) exceeded"),
                span,
            ));
        }
        Ok(())
    }

    /// Reads a field out of a struct value, following refstruct handles —
    /// the read half of `Expr::Field`. Clones the field out (the oracle
    /// trades copies for simplicity).
    fn get_field(&self, container: &Value, field: &str, span: Span) -> Result<Value, Diagnostic> {
        match container {
            Value::Struct { fields, .. } => get_in_fields(fields, field, span),
            Value::Ref(id) => get_in_fields(&self.heap.structs[*id].fields, field, span),
            other => Err(Diagnostic::error(
                format!("type {} has no fields", other.type_name()),
                span,
            )),
        }
    }

    /// Bounds-checked (cell, index) extraction shared by element reads and
    /// writes. The value shapes are checker-guaranteed; the bounds aren't.
    fn index_array(
        &self,
        array: Value,
        index: Value,
        span: Span,
    ) -> Result<(usize, usize), Diagnostic> {
        match (array, index) {
            (Value::Array(id), Value::Int(i)) => {
                let len = self.heap.arrays[id].len();
                match usize::try_from(i).ok().filter(|&i| i < len) {
                    Some(i) => Ok((id, i)),
                    None => Err(Diagnostic::error(
                        format!("index {i} out of bounds (length {len})"),
                        span,
                    )),
                }
            }
            (a, b) => Err(Diagnostic::error(
                format!("cannot index {} with {}", a.type_name(), b.type_name()),
                span,
            )),
        }
    }

    fn lookup(&mut self, name: &str, span: Span) -> Result<Value, Diagnostic> {
        self.slot_mut(name, span).map(|v| v.clone())
    }

    /// The scope slot holding `name`, innermost first.
    fn slot_mut(&mut self, name: &str, span: Span) -> Result<&mut Value, Diagnostic> {
        self.scopes
            .iter_mut()
            .rev()
            .find_map(|s| s.get_mut(name))
            .ok_or_else(|| Diagnostic::error(format!("undefined variable '{name}'"), span))
    }
}

/// Field lookup/update on a sorted fields vec — shared by inline structs
/// and heap objects. Error arms are defensive; the checker validated fields.
fn get_in_fields(fields: &[(String, Value)], field: &str, span: Span) -> Result<Value, Diagnostic> {
    fields
        .iter()
        .find(|(fname, _)| fname == field)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| Diagnostic::error(format!("no field '{field}'"), span))
}

fn set_in_fields(
    fields: &mut [(String, Value)],
    field: &str,
    v: Value,
    span: Span,
) -> Result<(), Diagnostic> {
    match fields.iter_mut().find(|(fname, _)| fname == field) {
        Some((_, slot)) => {
            *slot = v;
            Ok(())
        }
        None => Err(Diagnostic::error(format!("no field '{field}'"), span)),
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
        // The other idiv trap: i64::MIN / -1 has no i64 result (and would
        // panic Rust's `/` here).
        Div | Rem if a == i64::MIN && b == -1 => {
            return Err(Diagnostic::error("division overflow", span));
        }
        Div => Value::Int(a / b),
        Rem => Value::Int(a % b),
        Eq => Value::Bool(a == b),
        Ne => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Le => Value::Bool(a <= b),
        Gt => Value::Bool(a > b),
        Ge => Value::Bool(a >= b),
        And | Or | Coalesce => {
            unreachable!("short-circuiting operators are handled lazily in eval")
        }
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
        And | Or | Coalesce => {
            unreachable!("short-circuiting operators are handled lazily in eval")
        }
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
