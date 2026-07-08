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
    Ok(e.asm)
}

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

/// Slots needed for a body: one per `let` site, blocks included.
fn count_lets(body: &[Stmt]) -> usize {
    body.iter()
        .map(|stmt| match stmt {
            Stmt::Let { .. } => 1,
            Stmt::If {
                then_body,
                else_body,
                ..
            } => count_lets(then_body) + else_body.as_deref().map_or(0, count_lets),
            Stmt::While { body, .. } | Stmt::For { body, .. } => count_lets(body),
            _ => 0,
        })
        .sum()
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
    fn function(&mut self, f: &Function, module: usize) -> Result<(), Diagnostic> {
        for p in &f.params {
            if !matches!(p.ty, TypeAnn::Int | TypeAnn::Bool) {
                return Err(unsupported("parameters of this type", f.span));
            }
        }
        if !matches!(
            f.return_type,
            None | Some(TypeAnn::Int) | Some(TypeAnn::Bool)
        ) {
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
        let frame = ((f.params.len() + count_lets(&f.body)) * 8 + 15) & !15;
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

    fn lookup(&self, name: &str) -> Option<i64> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn block(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        self.scopes.push(HashMap::new());
        let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
        self.scopes.pop();
        result
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Diagnostic> {
        match stmt {
            Stmt::Let { name, value, .. } => {
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
            Stmt::Expr(expr) => self.expr(expr)?, // value discarded
            other => return Err(unsupported("this statement", other.span())),
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
            Expr::Binary { op, lhs, rhs, span } => {
                // lhs in %rax, rhs in %rcx. Wrapping add/sub/mul match the
                // interpreter's wrapping ops; idiv truncates toward zero
                // and signs the remainder like the dividend, matching the
                // oracle on every input it runs cleanly (it diagnoses the
                // idiv traps: /0 and i64::MIN / -1). Comparisons: cmpq
                // computes rax - rcx, so the set condition reads lhs ? rhs.
                let apply = match op {
                    BinOp::Add => "\taddq %rcx, %rax\n",
                    BinOp::Sub => "\tsubq %rcx, %rax\n",
                    BinOp::Mul => "\timulq %rcx, %rax\n",
                    BinOp::Div => "\tcqto\n\tidivq %rcx\n",
                    BinOp::Rem => "\tcqto\n\tidivq %rcx\n\tmovq %rdx, %rax\n",
                    BinOp::Eq => CMP_SETE,
                    BinOp::Ne => CMP_SETNE,
                    BinOp::Lt => CMP_SETL,
                    BinOp::Le => CMP_SETLE,
                    BinOp::Gt => CMP_SETG,
                    BinOp::Ge => CMP_SETGE,
                    BinOp::And | BinOp::Or => unreachable!("handled above"),
                    BinOp::Coalesce => {
                        return Err(unsupported("operator '??'", *span));
                    }
                };
                self.expr(lhs)?;
                self.push("%rax");
                self.expr(rhs)?;
                self.asm.push_str("\tmovq %rax, %rcx\n");
                self.pop("%rax");
                self.asm.push_str(apply);
            }
            Expr::Call { callee, args, span } => {
                let Expr::Ident(name, _) = callee.as_ref() else {
                    return Err(unsupported("this callee", *span));
                };
                let Some((tm, tname)) = self.res.functions[self.module].get(name) else {
                    // Resolution order says: no user definition, so this
                    // is a builtin (print/len/push) — they need runtime
                    // support that doesn't exist yet.
                    return Err(unsupported(&format!("builtin '{name}'"), *span));
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
                // The ABI wants %rsp 16-aligned at the call instruction;
                // an odd number of pending operand pushes leaves it 8 off.
                let misaligned = self.depth % 2 == 1;
                if misaligned {
                    self.asm.push_str("\tsubq $8, %rsp\n");
                }
                let _ = writeln!(self.asm, "\tcall {}", label_of(*tm, tname));
                if misaligned {
                    self.asm.push_str("\taddq $8, %rsp\n");
                }
            }
            other => return Err(unsupported("this expression", other.span())),
        }
        Ok(())
    }
}

const CMP_SETE: &str = "\tcmpq %rcx, %rax\n\tsete %al\n\tmovzbq %al, %rax\n";
const CMP_SETNE: &str = "\tcmpq %rcx, %rax\n\tsetne %al\n\tmovzbq %al, %rax\n";
const CMP_SETL: &str = "\tcmpq %rcx, %rax\n\tsetl %al\n\tmovzbq %al, %rax\n";
const CMP_SETLE: &str = "\tcmpq %rcx, %rax\n\tsetle %al\n\tmovzbq %al, %rax\n";
const CMP_SETG: &str = "\tcmpq %rcx, %rax\n\tsetg %al\n\tmovzbq %al, %rax\n";
const CMP_SETGE: &str = "\tcmpq %rcx, %rax\n\tsetge %al\n\tmovzbq %al, %rax\n";

fn unsupported(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}
