use std::collections::HashMap;

use crate::ast::{BinOp, Expr, Function, Item, Stmt, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;
use crate::syntax;

// ---- Interpreter policy ----------------------------------------------
// One unit of evaluation depth (a call, statement, or expression level)
// costs at most ~16KB of native stack in debug builds (measured); the
// owned stack is sized so the depth budget always binds first:
// 65_536 units x 16KB = 1GB = INTERP_STACK_BYTES. The heap cap turns
// runaway allocation into a diagnostic instead of an OOM kill.
const MAX_EVAL_DEPTH: usize = 65_536;
const MAX_HEAP_CELLS: usize = 1 << 20;
const INTERP_STACK_BYTES: usize = 1 << 30;

/// The interpreter's arena: every refstruct object and array buffer lives
/// here, addressed by handle into its own typed table — a `Value::Ref` can
/// only name a struct object and a `Value::Array` only a buffer, so no
/// mismatch arm exists anywhere. Nothing is freed mid-run; the arena drops
/// wholesale when execution ends (ADR 0009's collector-free story), which
/// also makes reference cycles harmless.
#[derive(Debug, Default)]
pub struct Heap {
    structs: Vec<StructObj>,
    arrays: Vec<Vec<Value>>,
}

/// A refstruct object; fields sorted by name like inline structs.
#[derive(Debug)]
struct StructObj {
    name: String,
    fields: Vec<(String, Value)>,
}

impl Heap {
    fn cell_count(&self) -> usize {
        self.structs.len() + self.arrays.len()
    }
}

/// Handles are plain indices, so the derived `PartialEq` gives refstructs
/// and arrays identity equality for free, and `Value` stays `Send` — which
/// lets the interpreter own its execution stack (see `interpret`).
#[derive(Debug, Clone, PartialEq)]
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
    /// A `refstruct` instance: a handle to one shared heap object, aliased
    /// by every copy of the handle.
    Ref(usize),
    /// An array: a handle to one shared, growable heap buffer.
    Array(usize),
    /// The `null` literal — the empty state of a `T?` slot.
    Null,
    Unit,
}

impl Value {
    /// Human-facing rendering for `print`: scalars and strings raw, structs
    /// and arrays in source-like shape, refs printed through the handle.
    /// Depth-capped like `render` — a handle hop costs a level, so cycles
    /// stay as bounded as they were under the Rc oracle.
    pub fn display(&self, heap: &Heap) -> String {
        self.display_depth(heap, 8)
    }

    fn display_depth(&self, heap: &Heap, depth: usize) -> String {
        if depth == 0 {
            return "...".to_string();
        }
        match self {
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s.clone(),
            Value::Null => "null".to_string(),
            Value::Unit => "unit".to_string(),
            // The hop consumes a level, the object's children another.
            Value::Ref(id) if depth == 1 => {
                let _ = id;
                "...".to_string()
            }
            Value::Ref(id) => {
                let obj = &heap.structs[*id];
                render_struct(&obj.name, &obj.fields, |v| v.display_depth(heap, depth - 2))
            }
            Value::Array(id) => {
                render_items(&heap.arrays[*id], |v| v.display_depth(heap, depth - 1))
            }
            Value::Struct { name, fields } => {
                render_struct(name, fields, |v| v.display_depth(heap, depth - 1))
            }
        }
    }

    /// Debug-style rendering with a depth cap — cyclic refstruct values
    /// would recurse forever otherwise.
    // ponytail: depth cap, not cycle detection — 8 levels is plenty for a
    // result dump; switch to handle-tracking if real output needs it.
    pub fn render(&self, heap: &Heap) -> String {
        self.render_depth(heap, 8)
    }

    fn render_depth(&self, heap: &Heap, depth: usize) -> String {
        if depth == 0 {
            return "...".to_string();
        }
        match self {
            Value::Ref(_) if depth == 1 => "...".to_string(),
            Value::Ref(id) => {
                let obj = &heap.structs[*id];
                format!(
                    "Ref({})",
                    render_struct(&obj.name, &obj.fields, |v| v.render_depth(heap, depth - 2))
                )
            }
            Value::Array(id) => {
                render_items(&heap.arrays[*id], |v| v.render_depth(heap, depth - 1))
            }
            Value::Struct { name, fields } => {
                render_struct(name, fields, |v| v.render_depth(heap, depth - 1))
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
            Value::Array(_) => "array",
            Value::Null => "null",
            Value::Unit => "unit",
        }
    }
}

fn render_struct(name: &str, fields: &[(String, Value)], mut one: impl FnMut(&Value) -> String) -> String {
    let fields: Vec<String> = fields.iter().map(|(f, v)| format!("{f}: {}", one(v))).collect();
    format!("{name} {{ {} }}", fields.join(", "))
}

fn render_items(items: &[Value], one: impl FnMut(&Value) -> String) -> String {
    let items: Vec<String> = items.iter().map(one).collect();
    format!("[{}]", items.join(", "))
}

/// Runs `main()` from the entry module (graph index 0), resolving every call
/// through its module's alias map. Returns `Unit` (and the heap, for
/// rendering the result) when there is no `main`. Execution happens on the
/// interpreter's own thread — `Value` is `Send` because handles are plain
/// arena indices.
pub fn interpret(
    graph: &ModuleGraph,
    resolutions: &Resolutions,
) -> Result<(Value, Heap), Diagnostic> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("interpreter".to_string())
            .stack_size(INTERP_STACK_BYTES)
            .spawn_scoped(scope, || run_program(graph, resolutions));
        match worker {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            // Constrained hosts (strict overcommit, tight rlimits) can
            // refuse the stack reservation — that's an error, not a panic.
            Err(e) => Err(Diagnostic::error(
                format!("cannot start the interpreter: {e}"),
                Span::new(0, 0),
            )),
        }
    })
}

fn run_program(
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
    let value = match interp.functions.get(&(0, "main")).copied() {
        Some(main) => interp.call(main, 0, Vec::new(), Span::new(0, 0))?,
        None => Value::Unit,
    };
    Ok((value, interp.heap))
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
                            format!(
                                "can only iterate over arrays, found {}",
                                other.type_name()
                            ),
                            *span,
                        ))
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
                    if let Flow::Return(v) = flow? {
                        return Ok(Flow::Return(v));
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
                            Value::Array(id) => {
                                Ok(Value::Int(self.heap.arrays[id].len() as i64))
                            }
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
                Value::Ref(id) => {
                    set_in_fields(&mut self.heap.structs[id].fields, name, v, *span)
                }
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
        run_full(src).map(|(value, _)| value)
    }

    /// Like `run`, but keeps the heap for rendering assertions.
    fn run_full(src: &str) -> Result<(Value, Heap), Diagnostic> {
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
        interpret(&graph, &res).map(|(value, _)| value)
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
            run("fun main(): int { const x: int = 10; return -x + 2; }"),
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
    fn for_loops_iterate_and_return_early() {
        let program = "\
fun find(xs: int[], needle: int): bool {
    for x in xs { if x == needle { return true; } }
    return false;
}
fun main(): int {
    var xs: int[] = [3, 7, 42];
    var total: int = 0;
    for x in xs { total = total + x; }
    if find(xs, 7) && !find(xs, 9) { return total; }
    return 0;
}";
        assert_eq!(run(program), Ok(Value::Int(52)));
    }

    #[test]
    fn widened_iterables_run_with_optional_elements() {
        let program = "\
fun main(): int {
    var acc: int = 0;
    for x in [10, null, 32] { acc = acc + (x ?? 0); }
    return acc;
}";
        assert_eq!(run(program), Ok(Value::Int(42)));
    }

    #[test]
    fn for_loops_track_the_index_on_request() {
        let program = "\
fun main(): int {
    var xs: int[] = [10, 20, 30];
    var acc: int = 0;
    for [i, x] in xs { acc = acc + i * x; }
    return acc;
}";
        // 0*10 + 1*20 + 2*30
        assert_eq!(run(program), Ok(Value::Int(80)));
    }

    #[test]
    fn arrays_roundtrip_with_builtins() {
        let program = "\
fun main(): int {
    var xs: int[] = [];
    push(xs, 10);
    push(xs, 20);
    push(xs, 12);
    xs[2] = xs[2] + 0;
    var i: int = 0;
    var sum: int = 0;
    while i < len(xs) { sum = sum + xs[i]; i = i + 1; }
    return sum;
}";
        assert_eq!(run(program), Ok(Value::Int(42)));
    }

    #[test]
    fn arrays_alias_and_compare_by_identity() {
        let program = "\
fun main(): bool {
    const a: int[] = [1, 2];
    const b: int[] = a;
    b[0] = 9;
    return a[0] == 9 && a == b && a != [1, 2];
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn out_of_bounds_indexing_is_a_runtime_error() {
        let result = run("fun main(): int { const xs: int[] = [1]; return xs[5]; }");
        assert!(
            result.as_ref().is_err_and(|e| e.message.contains("out of bounds")),
            "{result:?}"
        );
    }

    #[test]
    fn value_structs_in_arrays_write_back_through_the_index() {
        let program = "\
struct P { x: int }
fun main(): int {
    var ps: P[] = [P { x: 1 }];
    ps[0].x = 7;
    return ps[0].x;
}";
        assert_eq!(run(program), Ok(Value::Int(7)));
    }

    #[test]
    fn print_runs_and_returns_unit() {
        assert_eq!(
            run("fun main() { print(\"hi\"); print(1 + 1); print(null == null); }"),
            Ok(Value::Unit)
        );
    }

    #[test]
    fn user_print_shadows_the_builtin_at_runtime() {
        assert_eq!(
            run("fun print(n: int): int { return n * 2; }\n\
                 fun main(): int { return print(21); }"),
            Ok(Value::Int(42))
        );
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
    const a: int = square(3);
    var b: int = 4;
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
    var i: int = 0;
    var acc: int = 0;
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
    var x: int = 1;
    if true { const x: int = 10; }
    return x;
}";
        assert_eq!(run(program), Ok(Value::Int(1)));
    }

    #[test]
    fn struct_literal_and_field_access() {
        let program = "\
struct Point { x: int, y: int }
fun main(): int {
    const p: Point = Point { x: 3, y: 4 };
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
    const o: Outer = Outer { i: Inner { v: 7 } };
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
                     fun main(): int { const p: Pair = make(); return p.a + Pair { a: 1, b: 2 }.b; }"
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
    const a: Point = Point { x: 1, y: 2 };
    const b: Point = Point { x: 1, y: 2 };
    const c: Point = Point { x: 9, y: 2 };
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
    const a: Outer = Outer { i: Inner { v: 1 } };
    const b: Outer = Outer { i: Inner { v: 2 } };
    return a != b;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn field_assignment_mutates_the_struct() {
        let program = "\
struct Point { x: int, y: int }
fun main(): int {
    var p: Point = Point { x: 1, y: 2 };
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
    var o: Outer = Outer { i: Inner { v: 1 } };
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
    const a: P = P { x: 1 };
    const b: P = a;
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
    const p: P = P { x: 1 };
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
    const a: P = P { x: 1 };
    const b: P = P { x: 1 };
    const c: P = a;
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
    var a: V = V { x: 1 };
    var b: V = a;
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
    const r1: R = R { v: 1 };
    const r2: R = R { v: 1 };
    const a: Box = Box { r: r1 };
    const b: Box = Box { r: r1 };
    const c: Box = Box { r: r2 };
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
    const b: Box = Box { r: R { v: 1 } };
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
                     fun main(): int { const c: Counter = Counter { n: 0 }; bump(c); bump(c); return c.n; }"
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
    const was_null: bool = p == null;
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
    const head: Node = Node { v: 1, next: Node { v: 2, next: Node { v: 3, next: null } } };
    var cur: Node? = head;
    var sum: int = 0;
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
    const a: Node = Node { v: 1, next: null };
    a.next = a;
    return a;
}";
        let (value, heap) = run_full(program).unwrap();
        let rendered = value.render(&heap);
        assert!(rendered.contains("Node") && rendered.contains("..."), "{rendered}");
        assert!(rendered.len() < 500, "unbounded: {} bytes", rendered.len());
    }

    #[test]
    fn scalar_rendering_matches_debug() {
        let heap = Heap::default();
        assert_eq!(Value::Int(55).render(&heap), "Int(55)");
        assert_eq!(Value::Bool(true).render(&heap), "Bool(true)");
    }

    #[test]
    fn display_shows_user_values_not_enum_internals() {
        let mut heap = Heap::default();
        heap.arrays.push(vec![Value::Int(1), Value::Str("x".into()), Value::Null]);
        let array = Value::Array(0);
        assert_eq!(array.display(&heap), "[1, x, null]");
        let s = Value::Struct {
            name: "P".into(),
            fields: vec![("x".into(), Value::Int(1))],
        };
        assert_eq!(s.display(&heap), "P { x: 1 }");
        heap.structs.push(StructObj {
            name: "N".into(),
            fields: vec![("v".into(), Value::Bool(true))],
        });
        let r = Value::Ref(0);
        assert_eq!(r.display(&heap), "N { v: true }");
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
    var cur: Tree = t;
    while cur.left != null { cur = cur.left; }
    return cur.v;
}
fun main(): bool {
    var root: Tree? = null;
    root = insert(root, 5);
    const keep: Tree? = root;
    root = insert(root, 3);
    root = insert(root, 8);
    root = insert(root, 1);
    return sum(root) == 17 && keep == root && min(keep ?? insert(null, 0)) == 1;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn field_narrowed_traversal_runs() {
        let program = "\
refstruct Node { v: int, next: Node? }
fun last(head: Node): int {
    var cur: Node = head;
    while cur.next != null { cur = cur.next; }
    return cur.v;
}
fun main(): int {
    return last(Node { v: 1, next: Node { v: 2, next: Node { v: 3, next: null } } });
}";
        assert_eq!(run(program), Ok(Value::Int(3)));
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
    const a: P = P { x: 1 };
    return same(a) == a;
}";
        assert_eq!(run(program), Ok(Value::Bool(true)));
    }

    #[test]
    fn deep_expressions_with_recursion_are_a_diagnostic_not_a_crash() {
        // Expression nesting recurses natively too — it must draw from the
        // same depth budget as calls instead of overflowing the stack.
        let program = "\
fun down(n: int): int {
    if n == 0 { return 0; }
    return 0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + down(n - 1))))))))));
}
fun main(): int { return down(100000); }";
        let result = run(program);
        assert!(
            result.as_ref().is_err_and(|e| e.message.contains("depth limit")),
            "{result:?}"
        );
    }

    #[test]
    fn runaway_allocation_is_a_diagnostic_not_an_oom() {
        // Loop temporaries land in the arena; a cap turns runaway
        // allocation into a sanctioned diagnostic instead of an OOM kill.
        let program = "\
fun main() {
    var i: int = 0;
    while i < 400000 {
        const xs: int[][] = [[1], [2], [3], [4]];
        i = i + 1;
    }
}";
        let result = run(program);
        assert!(
            result.as_ref().is_err_and(|e| e.message.contains("heap limit")),
            "{result:?}"
        );
    }

    #[test]
    fn ref_hops_consume_render_depth() {
        // A two-ref-field cycle must stay bounded like the Rc oracle did
        // (hop + struct each cost a level), not fan out exponentially.
        let program = "\
refstruct T { a: T?, b: T? }
fun main(): T {
    const t: T = T { a: null, b: null };
    t.a = t;
    t.b = t;
    return t;
}";
        let (value, heap) = run_full(program).unwrap();
        let rendered = value.render(&heap);
        assert!(rendered.len() < 600, "fan-out: {} bytes", rendered.len());
    }

    #[test]
    fn runaway_recursion_is_a_diagnostic_not_a_crash() {
        let result = run("fun f(): int { return f(); }\nfun main(): int { return f(); }");
        assert!(
            result.as_ref().is_err_and(|e| e.message.contains("depth limit")),
            "{result:?}"
        );
    }

    #[test]
    fn deep_but_bounded_recursion_still_runs() {
        let program = "\
fun down(n: int): int { if n == 0 { return 0; } return down(n - 1); }
fun main(): int { return down(4000); }";
        assert_eq!(run(program), Ok(Value::Int(0)));
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
