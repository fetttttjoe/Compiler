//! x86-64 backend (ADR 0009): compiles the program to AT&T assembly for
//! the system `cc` to assemble and link. Covers int/bool arithmetic,
//! comparisons, short-circuit logic, locals, `if`/`while`, and direct
//! calls (≤6 register args, System V). Everything else is a clean
//! "not yet compilable" diagnostic; breadth arrives slice by slice,
//! each diffed against the interpreter (see tests/diff.rs).
//!
//! Scheme: recursive emission into %rax, machine stack for the pending
//! left operand. ponytail: no IR and no register allocation until a real
//! optimization needs them, per ADR 0009.
//!
//! Compiled behavior on the idiv traps — division by zero and
//! i64::MIN / -1 — is deferred (the binary takes a SIGFPE): the
//! interpreter diagnoses both, and the differential harness only diffs
//! programs the interpreter runs cleanly.
//!
//! Standing obligations for later slices (so they aren't rediscovered
//! as bugs): the first emitted `call` must keep %rsp 16-byte aligned at
//! the call site (System V ABI — SSE spills fault without it), and the
//! first data symbol (string literal, global) must use RIP-relative
//! addressing, because the system cc links PIE by default.

use crate::ast::{BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;
use crate::types::Type;
use std::collections::HashMap;
use std::fmt::Write;

/// System V integer argument registers, in order.
const ARG_REGS: [&str; 6] = ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"];

/// Compiles the checked program to assembly text: every function in every
/// module (like a C translation unit — an unreachable function must still
/// compile), calls resolved through the same alias maps the interpreter
/// uses. `main_fn` is the entry module's `main`, already verified to
/// exist by the caller.
pub fn compile(
    main_fn: &Function,
    graph: &ModuleGraph,
    res: &Resolutions,
) -> Result<String, Diagnostic> {
    if main_fn.return_type != Some(TypeAnn::Int) {
        return Err(unsupported("main not returning int", main_fn.span));
    }

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut e = Emitter {
        asm: String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n"),
        scopes: Vec::new(),
        next_slot: 0,
        labels: 0,
        depth: 0,
        module: 0,
        res,
    };
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                e.function(f, mi)?;
            }
        }
    }
    e.asm.push_str(RUNTIME);
    Ok(e.asm)
}

/// The in-assembly runtime, appended to every program. Arrays follow ADR
/// 0014: a handle points at a `{len, cap, data*}` header, elements are
/// inline 8-byte values, buffers come from libc malloc/realloc and are
/// never freed (the arena/leak story of ADR 0009/0015). `ys_push` grows
/// by doubling (min 4). The label can't collide with user code — every
/// user symbol except the entry `main` carries a `_<module>` suffix.
const RUNTIME: &str = "\
ys_push:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq 0(%rdi), %rax
\tcmpq 8(%rdi), %rax
\tjb .Lys_push_store
\tmovq 8(%rdi), %rcx
\ttestq %rcx, %rcx
\tjne .Lys_push_double
\tmovq $2, %rcx
.Lys_push_double:
\taddq %rcx, %rcx
\tmovq %rcx, 8(%rdi)
\tpushq %rdi
\tpushq %rsi
\tleaq 0(,%rcx,8), %rsi
\tmovq 16(%rdi), %rdi
\tcall realloc@PLT
\tpopq %rsi
\tpopq %rdi
\tmovq %rax, 16(%rdi)
\tmovq 0(%rdi), %rax
.Lys_push_store:
\tmovq 16(%rdi), %rcx
\tmovq %rsi, (%rcx,%rax,8)
\tincq %rax
\tmovq %rax, 0(%rdi)
\tpopq %rbp
\tret
";

/// The assembly symbol for a function: the entry `main` keeps its name
/// (the C runtime calls it); everything else is suffixed with its module
/// index, which decodes uniquely (the suffix after the last underscore).
fn label_of(module: usize, name: &str) -> String {
    if module == 0 && name == "main" {
        name.to_string()
    } else {
        format!("{name}_{module}")
    }
}

/// Peak number of simultaneously live `let` slots for a body. Blocks
/// release their slots on exit (`block` restores `next_slot`), so sibling
/// scopes share: the frame needs the deepest path, not the sum. `for`
/// bindings (element + optional index) are counted with their body so the
/// frame is already right when that slice lands.
fn peak_slots(body: &[Stmt]) -> usize {
    let mut live = 0;
    let mut peak = 0;
    for stmt in body {
        let inner = match stmt {
            Stmt::Let { .. } => {
                live += 1;
                0
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => peak_slots(then_body).max(else_body.as_deref().map_or(0, peak_slots)),
            Stmt::While { body, .. } => peak_slots(body),
            // Element + counter (doubles as the index binding) + the
            // evaluated-once array handle.
            Stmt::For { body, .. } => 3 + peak_slots(body),
            _ => 0,
        };
        peak = peak.max(live + inner);
    }
    peak.max(live)
}

struct Emitter<'a> {
    asm: String,
    /// Innermost-last scope stack; each block pushes and pops one frame.
    scopes: Vec<HashMap<String, i64>>,
    /// Slots handed out so far — every `let` site takes a fresh one.
    next_slot: i64,
    /// Global label counter — jump labels must be unique per file.
    labels: usize,
    /// Outstanding operand pushes. %rsp sits 16-aligned at statement
    /// level, so this parity decides the call-site alignment fix-up.
    depth: usize,
    /// The module whose function is being emitted — call names resolve
    /// through its alias map.
    module: usize,
    res: &'a Resolutions,
}

impl Emitter<'_> {
    /// Emits one function: prologue, register-spilled params, body,
    /// fall-through epilogue (reachable only for unit functions — the
    /// checker proves value-returning bodies always return).
    /// The `TypeAnn` mirror of `word_type`, for annotations (params,
    /// returns, `let`) resolved through this module's visible names.
    fn word_ann(&self, ty: &TypeAnn, module: usize) -> bool {
        match ty {
            TypeAnn::Int | TypeAnn::Bool => true,
            TypeAnn::Named(n) => self.res.ref_structs[module].contains(n),
            // Element annotations recurse: `int?[]` is as uncompilable
            // as a bare `int?` slot.
            TypeAnn::Array(inner) => self.word_ann(inner, module),
            TypeAnn::Optional(inner) => match inner.as_ref() {
                TypeAnn::Named(n) => self.res.ref_structs[module].contains(n),
                TypeAnn::Array(_) => true,
                _ => false,
            },
            _ => false,
        }
    }

    fn function(&mut self, f: &Function, module: usize) -> Result<(), Diagnostic> {
        for p in &f.params {
            if !self.word_ann(&p.ty, module) {
                return Err(unsupported("parameters of this type", f.span));
            }
        }
        if !f
            .return_type
            .as_ref()
            .is_none_or(|t| self.word_ann(t, module))
        {
            return Err(unsupported("this return type", f.span));
        }
        if f.params.len() > ARG_REGS.len() {
            return Err(unsupported("more than 6 parameters", f.span));
        }

        self.module = module;
        self.next_slot = 0;
        self.depth = 0;
        let label = label_of(module, &f.name);
        if label == "main" {
            self.asm.push_str("\t.globl main\n");
        }
        let _ = writeln!(self.asm, "{label}:");

        // Slots are rbp-relative because operand pushes move %rsp; the
        // frame is 16-byte aligned so %rsp parity at any point is just
        // the outstanding push count.
        self.asm.push_str("\tpushq %rbp\n\tmovq %rsp, %rbp\n");
        let frame = ((f.params.len() + peak_slots(&f.body)) * 8 + 15) & !15;
        if frame > 0 {
            let _ = writeln!(self.asm, "\tsubq ${frame}, %rsp");
        }

        let mut params = HashMap::new();
        for (i, p) in f.params.iter().enumerate() {
            self.next_slot -= 8;
            let _ = writeln!(self.asm, "\tmovq {}, {}(%rbp)", ARG_REGS[i], self.next_slot);
            params.insert(p.name.clone(), self.next_slot);
        }
        self.scopes = vec![params];

        for stmt in &f.body {
            self.stmt(stmt)?;
        }
        self.asm.push_str("\tleave\n\tret\n");
        Ok(())
    }

    fn fresh_label(&mut self) -> String {
        self.labels += 1;
        format!(".L{}", self.labels)
    }

    fn push(&mut self, reg: &str) {
        self.depth += 1;
        let _ = writeln!(self.asm, "\tpushq {reg}");
    }

    fn pop(&mut self, reg: &str) {
        self.depth -= 1;
        let _ = writeln!(self.asm, "\tpopq {reg}");
    }

    /// Emits a call with the ABI's 16-byte %rsp alignment: an odd number
    /// of pending operand pushes leaves %rsp 8 off, fixed up around the
    /// call. Used for user functions, the runtime, and libc alike.
    fn call(&mut self, symbol: &str) {
        let misaligned = self.depth % 2 == 1;
        if misaligned {
            self.asm.push_str("\tsubq $8, %rsp\n");
        }
        let _ = writeln!(self.asm, "\tcall {symbol}");
        if misaligned {
            self.asm.push_str("\taddq $8, %rsp\n");
        }
    }

    /// ADR 0008's runtime bounds check: index in %rcx against the length
    /// of the array whose handle is in `hdl`. Unsigned compare catches
    /// negatives; out of bounds aborts — the deferred-trap policy (like
    /// SIGFPE for idiv), never a silent wild access.
    fn bounds_check(&mut self, hdl: &str) {
        let ok = self.fresh_label();
        let _ = writeln!(self.asm, "\tcmpq 0({hdl}), %rcx\n\tjb {ok}");
        self.call("abort@PLT");
        let _ = writeln!(self.asm, "{ok}:");
    }

    fn lookup(&self, name: &str) -> Option<i64> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn block(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        self.scopes.push(HashMap::new());
        let saved_slot = self.next_slot;
        let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
        // The block's names died with its scope, so its slots are free
        // for sibling blocks — this is what lets `peak_slots` size the
        // frame by deepest path instead of total let count.
        self.next_slot = saved_slot;
        self.scopes.pop();
        result
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Diagnostic> {
        match stmt {
            Stmt::Let {
                name, value, ty, ..
            } => {
                if let Some(ann) = ty {
                    if !self.word_ann(ann, self.module) {
                        return Err(unsupported("bindings of this type", stmt.span()));
                    }
                }
                self.expr(value)?;
                self.next_slot -= 8;
                let off = self.next_slot;
                self.scopes
                    .last_mut()
                    .expect("a scope is always open")
                    .insert(name.clone(), off);
                let _ = writeln!(self.asm, "\tmovq %rax, {off}(%rbp)");
            }
            Stmt::Assign { target, value, .. } => match target {
                Expr::Ident(name, span) => {
                    let Some(off) = self.lookup(name) else {
                        return Err(unsupported("this assignment target", *span));
                    };
                    self.expr(value)?;
                    let _ = writeln!(self.asm, "\tmovq %rax, {off}(%rbp)");
                }
                Expr::Index { base, index, .. } => {
                    self.expr(base)?;
                    self.push("%rax");
                    self.expr(index)?;
                    self.push("%rax");
                    self.expr(value)?;
                    self.pop("%rcx");
                    self.pop("%rdx");
                    self.bounds_check("%rdx");
                    self.asm
                        .push_str("\tmovq 16(%rdx), %rdx\n\tmovq %rax, (%rdx,%rcx,8)\n");
                }
                // `?.` links are not places (the parser rejects them), so
                // a field target is always a plain, checker-proven-safe
                // dereference: no null check needed.
                Expr::Field { base, span, .. } => {
                    let Some((slot, _)) = self.res.field_slots.get(span) else {
                        return Err(unsupported("this field target", *span));
                    };
                    let off = 8 * slot;
                    self.expr(base)?;
                    self.push("%rax");
                    self.expr(value)?;
                    self.pop("%rcx");
                    let _ = writeln!(self.asm, "\tmovq %rax, {off}(%rcx)");
                }
                other => return Err(unsupported("this assignment target", other.span())),
            },
            Stmt::Return { value, .. } => {
                if let Some(expr) = value {
                    self.expr(expr)?;
                }
                // `leave` restores %rsp from %rbp, so pending operand
                // pushes on this path unwind with the frame.
                self.asm.push_str("\tleave\n\tret\n");
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                let end = self.fresh_label();
                self.expr(cond)?;
                self.asm.push_str("\ttestq %rax, %rax\n");
                match else_body {
                    None => {
                        let _ = writeln!(self.asm, "\tje {end}");
                        self.block(then_body)?;
                    }
                    Some(else_body) => {
                        let otherwise = self.fresh_label();
                        let _ = writeln!(self.asm, "\tje {otherwise}");
                        self.block(then_body)?;
                        let _ = writeln!(self.asm, "\tjmp {end}\n{otherwise}:");
                        self.block(else_body)?;
                    }
                }
                let _ = writeln!(self.asm, "{end}:");
            }
            Stmt::While { cond, body, .. } => {
                let top = self.fresh_label();
                let end = self.fresh_label();
                let _ = writeln!(self.asm, "{top}:");
                self.expr(cond)?;
                self.asm.push_str("\ttestq %rax, %rax\n");
                let _ = writeln!(self.asm, "\tje {end}");
                self.block(body)?;
                let _ = writeln!(self.asm, "\tjmp {top}\n{end}:");
            }
            Stmt::For {
                index,
                name,
                iterable,
                body,
                ..
            } => {
                // The iterable evaluates once; iteration is live — length
                // re-read every step, element copied out before the body
                // runs — exactly the oracle's contract (interpreter.rs).
                // Three hidden slots: handle, counter (doubling as the
                // index binding), element. Released after the loop.
                let saved_slot = self.next_slot;
                self.expr(iterable)?;
                let (hdl, i, x) = (self.next_slot - 8, self.next_slot - 16, self.next_slot - 24);
                self.next_slot -= 24;
                let _ = writeln!(self.asm, "\tmovq %rax, {hdl}(%rbp)\n\tmovq $0, {i}(%rbp)");
                let top = self.fresh_label();
                let end = self.fresh_label();
                let _ = writeln!(
                    self.asm,
                    "{top}:\n\tmovq {hdl}(%rbp), %rax\n\tmovq {i}(%rbp), %rcx\n\
                     \tcmpq 0(%rax), %rcx\n\tjae {end}\n\
                     \tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax\n\
                     \tmovq %rax, {x}(%rbp)"
                );
                let mut bindings = HashMap::from([(name.clone(), x)]);
                if let Some(index) = index {
                    bindings.insert(index.clone(), i);
                }
                self.scopes.push(bindings);
                let inner_slot = self.next_slot;
                let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
                self.next_slot = inner_slot;
                self.scopes.pop();
                result?;
                let _ = writeln!(self.asm, "\tincq {i}(%rbp)\n\tjmp {top}\n{end}:");
                self.next_slot = saved_slot;
            }
            Stmt::Expr(expr) => self.expr(expr)?, // value discarded
        }
        Ok(())
    }

    /// Emits code leaving the expression's value in %rax (bools are 0/1).
    /// Binary ops park the left operand on the machine stack while the
    /// right side evaluates, then pop it into %rcx — pushes and pops
    /// always balance across every emitted path. Recursion depth is safe:
    /// the parser bounds AST height at construction (MAX_FN_OPS) and the
    /// pipeline runs on a worker stack sized for that bound (main.rs).
    fn expr(&mut self, expr: &Expr) -> Result<(), Diagnostic> {
        match expr {
            // movabsq takes a full 64-bit immediate; movq would cap at i32.
            Expr::Int(n, _) => {
                let _ = writeln!(self.asm, "\tmovabsq ${n}, %rax");
            }
            Expr::Bool(b, _) => {
                let _ = writeln!(self.asm, "\tmovq ${}, %rax", *b as i64);
            }
            // `null` is handle 0 — sound because value-typed optionals
            // never compile (annotation, param, field, and array-literal
            // gates), so 0 is never a legitimate optional payload.
            Expr::Null(_) => {
                self.asm.push_str("\txorl %eax, %eax\n");
            }
            Expr::Ident(name, span) => match self.lookup(name) {
                Some(off) => {
                    let _ = writeln!(self.asm, "\tmovq {off}(%rbp), %rax");
                }
                // The checker resolved it, but not to a local we can
                // compile yet (e.g. a function name used as a value).
                None => return Err(unsupported("this name", *span)),
            },
            Expr::Unary { op, rhs, .. } => {
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => self.asm.push_str("\tnegq %rax\n"),
                    // Bools are exactly 0 or 1, so `!` is one bit flip.
                    UnOp::Not => self.asm.push_str("\txorq $1, %rax\n"),
                }
            }
            Expr::Binary {
                op: BinOp::And,
                lhs,
                rhs,
                ..
            } => {
                // Short-circuit: a false left side IS the result (0), and
                // the right side must never run (the oracle is lazy — its
                // traps and effects must stay unreached here too).
                let end = self.fresh_label();
                self.expr(lhs)?;
                let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\tje {end}");
                self.expr(rhs)?;
                let _ = writeln!(self.asm, "{end}:");
            }
            Expr::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
                ..
            } => {
                let end = self.fresh_label();
                self.expr(lhs)?;
                let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\tjne {end}");
                self.expr(rhs)?;
                let _ = writeln!(self.asm, "{end}:");
            }
            Expr::Binary {
                op: BinOp::Coalesce,
                lhs,
                rhs,
                ..
            } => {
                // A null left side is handle 0 (value optionals never
                // compile); the right side stays lazy, like the oracle.
                let end = self.fresh_label();
                self.expr(lhs)?;
                let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\tjne {end}");
                self.expr(rhs)?;
                let _ = writeln!(self.asm, "{end}:");
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                // lhs in %rax, rhs in %rcx. Wrapping add/sub/mul match the
                // interpreter's wrapping ops; idiv truncates toward zero
                // and signs the remainder like the dividend, matching the
                // oracle on every input it runs cleanly (it diagnoses the
                // idiv traps: /0 and i64::MIN / -1).
                self.expr(lhs)?;
                self.push("%rax");
                self.expr(rhs)?;
                self.asm.push_str("\tmovq %rax, %rcx\n");
                self.pop("%rax");
                match op {
                    BinOp::Add => self.asm.push_str("\taddq %rcx, %rax\n"),
                    BinOp::Sub => self.asm.push_str("\tsubq %rcx, %rax\n"),
                    BinOp::Mul => self.asm.push_str("\timulq %rcx, %rax\n"),
                    BinOp::Div => self.asm.push_str("\tcqto\n\tidivq %rcx\n"),
                    BinOp::Rem => self
                        .asm
                        .push_str("\tcqto\n\tidivq %rcx\n\tmovq %rdx, %rax\n"),
                    // cmpq computes rax - rcx, so the condition code reads
                    // lhs ? rhs; one shared template keeps the six
                    // comparisons typo-proof.
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match op {
                            BinOp::Eq => "e",
                            BinOp::Ne => "ne",
                            BinOp::Lt => "l",
                            BinOp::Le => "le",
                            BinOp::Gt => "g",
                            BinOp::Ge => "ge",
                            _ => unreachable!(),
                        };
                        let _ = writeln!(
                            self.asm,
                            "\tcmpq %rcx, %rax\n\tset{cc} %al\n\tmovzbq %al, %rax"
                        );
                    }
                    BinOp::And | BinOp::Or | BinOp::Coalesce => {
                        unreachable!("handled above")
                    }
                }
            }
            Expr::Call { callee, args, span } => {
                let Expr::Ident(name, _) = callee.as_ref() else {
                    return Err(unsupported("this callee", *span));
                };
                let Some((tm, tname)) = self.res.functions[self.module].get(name) else {
                    // Resolution order says: no user definition, so this
                    // is a builtin.
                    return self.builtin(name, args, *span);
                };
                if args.len() > ARG_REGS.len() {
                    return Err(unsupported("calls with more than 6 arguments", *span));
                }
                // Evaluate args left to right onto the stack (a later
                // arg's subexpressions would clobber earlier registers),
                // then pop into the ABI registers in reverse.
                for arg in args {
                    self.expr(arg)?;
                    self.push("%rax");
                }
                for reg in ARG_REGS[..args.len()].iter().rev() {
                    self.pop(reg);
                }
                self.call(&label_of(*tm, tname));
            }
            Expr::ArrayLit { elements, span } => {
                // A null element could make the literal an `int?[]` —
                // a value-optional array the word model can't represent.
                // ponytail: over-strict for `Node?[]` literals too; build
                // those with push until the checker exports element types.
                if let Some(null) = elements.iter().find(|e| matches!(e, Expr::Null(_))) {
                    let _ = span;
                    return Err(unsupported(
                        "array literals with null elements",
                        null.span(),
                    ));
                }
                // Header {len, cap, data*} plus buffer, per ADR 0014.
                // Allocation happens before element evaluation — the
                // ordering difference from the oracle is only observable
                // through traps, which the harness excludes.
                let n = elements.len();
                self.asm.push_str("\tmovq $24, %rdi\n");
                self.call("malloc@PLT");
                self.push("%rax");
                let _ = writeln!(self.asm, "\tmovq ${}, %rdi", 8 * n.max(1));
                self.call("malloc@PLT");
                self.pop("%rcx");
                let _ = writeln!(
                    self.asm,
                    "\tmovq %rax, 16(%rcx)\n\tmovq ${n}, 0(%rcx)\n\tmovq ${n}, 8(%rcx)"
                );
                for (i, element) in elements.iter().enumerate() {
                    self.push("%rcx");
                    self.expr(element)?;
                    self.pop("%rcx");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq 16(%rcx), %rdx\n\tmovq %rax, {}(%rdx)",
                        8 * i
                    );
                }
                self.asm.push_str("\tmovq %rcx, %rax\n");
            }
            Expr::Index { base, index, .. } => {
                self.expr(base)?;
                self.push("%rax");
                self.expr(index)?;
                self.asm.push_str("\tmovq %rax, %rcx\n");
                self.pop("%rax");
                self.bounds_check("%rax");
                self.asm
                    .push_str("\tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax\n");
            }
            Expr::Field {
                base,
                optional,
                span,
                ..
            } => {
                let Some((slot, field_ty)) = self.res.field_slots.get(span) else {
                    return Err(unsupported("this field access", *span));
                };
                let off = 8 * slot;
                // `p?.x` with a value-typed x yields `int?` — a value
                // optional the word model can't represent. Handle-typed
                // fields are fine, already-optional ones stay flat.
                let nullable_word = match field_ty {
                    Type::Optional(inner) => ref_shaped(inner, self.res),
                    other => ref_shaped(other, self.res),
                };
                if *optional && !nullable_word {
                    return Err(unsupported("'?.' on a field of value type", *span));
                }
                self.expr(base)?;
                if *optional {
                    // Null short-circuits to null (0 stays in %rax).
                    let end = self.fresh_label();
                    let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\tje {end}");
                    let _ = writeln!(self.asm, "\tmovq {off}(%rax), %rax\n{end}:");
                } else {
                    // No null check: the checker's narrowing is sound, so
                    // a plain `.` base is proven non-null (ADR 0007).
                    let _ = writeln!(self.asm, "\tmovq {off}(%rax), %rax");
                }
            }
            Expr::StructLit { fields, span, .. } => {
                let Some(key) = self.res.struct_lits.get(span) else {
                    return Err(unsupported("this struct literal", *span));
                };
                let def = &self.res.structs[key];
                if !def.by_ref {
                    return Err(unsupported("value struct literals", *span));
                }
                if !def.fields.iter().all(|(_, t)| word_type(t, self.res)) {
                    return Err(unsupported("structs with fields of this type", *span));
                }
                // One heap object, fields at declaration-order word
                // offsets (ADR 0009's C-style layout). The checker proved
                // the literal complete, so every slot is written.
                let size = (def.fields.len() * 8).max(8);
                let _ = writeln!(self.asm, "\tmovq ${size}, %rdi");
                self.call("malloc@PLT");
                self.push("%rax");
                for (fname, value) in fields {
                    let slot = def
                        .fields
                        .iter()
                        .position(|(dn, _)| dn == fname)
                        .expect("checker verified the field exists");
                    self.expr(value)?;
                    self.pop("%rcx");
                    let _ = writeln!(self.asm, "\tmovq %rax, {}(%rcx)", 8 * slot);
                    self.push("%rcx");
                }
                self.pop("%rax");
            }
            other => return Err(unsupported("this expression", other.span())),
        }
        Ok(())
    }

    /// The compilable builtins. `len` is two loads; `push` calls the
    /// in-assembly runtime (its unit result is whatever's in %rax — the
    /// checker keeps unit values out of operand positions).
    fn builtin(&mut self, name: &str, args: &[Expr], span: Span) -> Result<(), Diagnostic> {
        match (name, args) {
            ("len", [array]) => {
                self.expr(array)?;
                self.asm.push_str("\tmovq 0(%rax), %rax\n");
            }
            ("push", [array, value]) => {
                self.expr(array)?;
                self.push("%rax");
                self.expr(value)?;
                self.asm.push_str("\tmovq %rax, %rsi\n");
                self.pop("%rdi");
                self.call("ys_push");
            }
            _ => return Err(unsupported(&format!("builtin '{name}'"), span)),
        }
        Ok(())
    }
}

/// A reference-shaped checker type: a handle where 0 means `null`, so a
/// `T?` of it is a nullable pointer for free (ADR 0009).
fn ref_shaped(t: &Type, res: &Resolutions) -> bool {
    match t {
        Type::Array(_) => true,
        Type::Struct(m, n) => res.structs[&(*m, n.clone())].by_ref,
        _ => false,
    }
}

/// A checker type the backend can hold in one word: scalars, handles,
/// and nullable handles. Value structs (multi-word) and value optionals
/// (0 and null would share a bit pattern) wait for their own slices.
fn word_type(t: &Type, res: &Resolutions) -> bool {
    match t {
        Type::Int | Type::Bool => true,
        Type::Optional(inner) => ref_shaped(inner, res),
        other => ref_shaped(other, res),
    }
}

fn unsupported(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}
