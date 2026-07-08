//! x86-64 backend, slice 1 (ADR 0009): compiles `main` returning an int
//! expression — literals, unary minus, `+ - * / %` — to AT&T assembly for
//! the system `cc` to assemble and link. Everything else is a clean
//! "not yet compilable" diagnostic; breadth arrives slice by slice, each
//! diffed against the interpreter (see tests/diff.rs).
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
use std::collections::HashMap;
use std::fmt::Write;

/// Compiles the checked program's `main` to assembly text.
pub fn compile(main: &Function) -> Result<String, Diagnostic> {
    if main.return_type != Some(TypeAnn::Int) {
        return Err(unsupported("main not returning int", main.span));
    }

    // One 8-byte rbp-relative slot per binding name. The body is flat
    // until control flow compiles (no blocks), so a same-scope shadow can
    // share its predecessor's name→slot entry: the initializer reads the
    // old value before the store rebinds it. Duplicates waste a slot.
    let locals: HashMap<&str, i64> = main
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Let { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .enumerate()
        .map(|(i, name)| (name, -8 * (i as i64 + 1)))
        .collect();

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut asm =
        String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n\t.globl main\nmain:\n");
    // Slots are rbp-relative because operand pushes move %rsp. Frame kept
    // 16-byte aligned — free now, mandatory once calls exist.
    asm.push_str("\tpushq %rbp\n\tmovq %rsp, %rbp\n");
    let frame = (locals.len() * 8 + 15) & !15;
    if frame > 0 {
        let _ = writeln!(asm, "\tsubq ${frame}, %rsp");
    }

    for stmt in &main.body {
        emit_stmt(stmt, &locals, &mut asm)?;
    }
    Ok(asm)
}

fn emit_stmt(stmt: &Stmt, locals: &HashMap<&str, i64>, asm: &mut String) -> Result<(), Diagnostic> {
    match stmt {
        Stmt::Let { name, value, .. } => {
            emit_expr(value, locals, asm)?;
            let off = locals[name.as_str()];
            let _ = writeln!(asm, "\tmovq %rax, {off}(%rbp)");
        }
        Stmt::Assign { target, value, .. } => match target {
            Expr::Ident(name, _) if locals.contains_key(name.as_str()) => {
                emit_expr(value, locals, asm)?;
                let off = locals[name.as_str()];
                let _ = writeln!(asm, "\tmovq %rax, {off}(%rbp)");
            }
            other => return Err(unsupported("this assignment target", other.span())),
        },
        Stmt::Return {
            value: Some(expr), ..
        } => {
            emit_expr(expr, locals, asm)?;
            asm.push_str("\tleave\n\tret\n");
        }
        Stmt::Expr(expr) => emit_expr(expr, locals, asm)?, // value discarded
        other => return Err(unsupported("this statement", other.span())),
    }
    Ok(())
}

/// Emits code leaving the expression's value in %rax. Binary ops park the
/// left operand on the machine stack while the right side evaluates, then
/// pop it into %rcx — pushes and pops always balance, so `ret` in
/// `compile` sees the frame it was called with. Recursion depth is safe:
/// the parser bounds AST height at construction (MAX_FN_OPS) and the
/// pipeline runs on a worker stack sized for that bound (main.rs).
fn emit_expr(expr: &Expr, locals: &HashMap<&str, i64>, asm: &mut String) -> Result<(), Diagnostic> {
    match expr {
        // movabsq takes a full 64-bit immediate; movq would cap at i32.
        Expr::Int(n, _) => {
            let _ = writeln!(asm, "\tmovabsq ${n}, %rax");
        }
        Expr::Ident(name, span) => match locals.get(name.as_str()) {
            Some(off) => {
                let _ = writeln!(asm, "\tmovq {off}(%rbp), %rax");
            }
            // The checker resolved it, but not to a local we can compile
            // yet (e.g. a function name used as a value).
            None => return Err(unsupported("this name", *span)),
        },
        Expr::Unary {
            op: UnOp::Neg, rhs, ..
        } => {
            emit_expr(rhs, locals, asm)?;
            asm.push_str("\tnegq %rax\n");
        }
        Expr::Binary { op, lhs, rhs, span } => {
            // lhs in %rax, rhs in %rcx. Wrapping add/sub/mul match the
            // interpreter's wrapping ops; idiv truncates toward zero and
            // signs the remainder like the dividend, matching the oracle
            // on every input the interpreter runs cleanly (it diagnoses
            // the idiv traps: /0 and i64::MIN / -1).
            let apply = match op {
                BinOp::Add => "\taddq %rcx, %rax\n",
                BinOp::Sub => "\tsubq %rcx, %rax\n",
                BinOp::Mul => "\timulq %rcx, %rax\n",
                BinOp::Div => "\tcqto\n\tidivq %rcx\n",
                BinOp::Rem => "\tcqto\n\tidivq %rcx\n\tmovq %rdx, %rax\n",
                _ => return Err(unsupported(&format!("operator '{}'", op.symbol()), *span)),
            };
            emit_expr(lhs, locals, asm)?;
            asm.push_str("\tpushq %rax\n");
            emit_expr(rhs, locals, asm)?;
            asm.push_str("\tmovq %rax, %rcx\n\tpopq %rax\n");
            asm.push_str(apply);
        }
        other => return Err(unsupported("this expression", other.span())),
    }
    Ok(())
}

fn unsupported(what: &str, span: crate::span::Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}
