//! x86-64 backend (ADR 0009): compiles `main` to AT&T assembly for the
//! system `cc` to assemble and link. Slice 3 covers int arithmetic,
//! bools, comparisons, short-circuit logic, locals, and `if`/`while`.
//! Everything else is a clean "not yet compilable" diagnostic; breadth
//! arrives slice by slice, each diffed against the interpreter
//! (see tests/diff.rs).
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

use crate::ast::{BinOp, Expr, Function, Stmt, TypeAnn, UnOp};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use std::collections::HashMap;
use std::fmt::Write;

/// Compiles the checked program's `main` to assembly text.
pub fn compile(main: &Function) -> Result<String, Diagnostic> {
    if main.return_type != Some(TypeAnn::Int) {
        return Err(unsupported("main not returning int", main.span));
    }

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut asm =
        String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n\t.globl main\nmain:\n");

    // Slots are rbp-relative because operand pushes move %rsp. Every
    // `let` site gets its own slot (blocks nest, so one-slot-per-name is
    // no longer sound); the frame stays 16-byte aligned — free now,
    // mandatory once calls exist.
    asm.push_str("\tpushq %rbp\n\tmovq %rsp, %rbp\n");
    let frame = (count_lets(&main.body) * 8 + 15) & !15;
    if frame > 0 {
        let _ = writeln!(asm, "\tsubq ${frame}, %rsp");
    }

    let mut e = Emitter {
        asm,
        scopes: vec![HashMap::new()],
        next_slot: 0,
        labels: 0,
    };
    for stmt in &main.body {
        e.stmt(stmt)?;
    }
    // Unreachable for int main (the checker proves every path returns),
    // but keeps a fall-through frame balanced once unit functions exist.
    e.asm.push_str("\tleave\n\tret\n");
    Ok(e.asm)
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

struct Emitter {
    asm: String,
    /// Innermost-last scope stack; each block pushes and pops one frame.
    scopes: Vec<HashMap<String, i64>>,
    /// Slots handed out so far — every `let` site takes a fresh one.
    next_slot: i64,
    labels: usize,
}

impl Emitter {
    fn fresh_label(&mut self) -> String {
        self.labels += 1;
        format!(".L{}", self.labels)
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
            Stmt::Return {
                value: Some(expr), ..
            } => {
                self.expr(expr)?;
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
                self.asm.push_str("\tpushq %rax\n");
                self.expr(rhs)?;
                self.asm.push_str("\tmovq %rax, %rcx\n\tpopq %rax\n");
                self.asm.push_str(apply);
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
