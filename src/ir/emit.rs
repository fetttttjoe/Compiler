//! IR → AT&T assembly, one small block per instruction. %rax/%rcx/%rdx
//! (and %xmm0/%xmm1) are permanent scratch — never allocated — so every
//! sequence may use them freely. The frame is laid out as
//! [saved callee regs | spills | multi-word temps], 16-byte aligned;
//! this backend never pushes operands, so %rsp stays aligned at every
//! call site with no fix-ups.

use super::regalloc::{ARG_REGS, CALLEE_SAVED, Loc, allocate, intervals};
use super::{FunctionIr, Inst, V, cc};
use crate::ast::BinOp;
use crate::codegen::{RT_FMOD, TRAP_DIV0, TRAP_OOB, TRAP_OVERFLOW, label_of};
use crate::syntax;
use std::collections::HashMap;
use std::fmt::Write;

/// Hacker's Delight 10-4: the multiplier and shift for signed division
/// by a positive constant. Valid for every i64 dividend, MIN included.
fn magic_i64(d: i64) -> (i64, u32) {
    debug_assert!(d >= 2);
    let ad = d as u128;
    let two63: u128 = 1 << 63;
    let anc = two63 - 1 - (two63 % ad);
    let mut p: u32 = 63;
    let mut q1 = two63 / anc;
    let mut r1 = two63 - q1 * anc;
    let mut q2 = two63 / ad;
    let mut r2 = two63 - q2 * ad;
    loop {
        p += 1;
        q1 *= 2;
        r1 *= 2;
        if r1 >= anc {
            q1 += 1;
            r1 -= anc;
        }
        q2 *= 2;
        r2 *= 2;
        if r2 >= ad {
            q2 += 1;
            r2 -= ad;
        }
        let delta = ad - r2;
        if q1 >= delta && !(q1 == delta && r1 == 0) {
            break;
        }
    }
    ((q2 + 1) as u64 as i64, p - 64)
}

/// The bounds-check preamble shared by element reads and writes:
/// an index below the length falls through to `target`; out-of-range
/// reports and exits through the OOB trap (ADR 0022).
fn bounds_check(arr: &str, idx: &str, loc: &str, target: &str) -> String {
    format!(
        "\tmovq {arr}, %rax\n\tmovq {idx}, %rcx\n\tcmpq 0(%rax), %rcx\n\
         \tjb {target}\n\
         \tmovq %rcx, %rdi\n\tmovq 0(%rax), %rsi\n\tleaq {loc}(%rip), %rdx\n\
         \tcall {TRAP_OOB}\n\
         {target}:\n"
    )
}

fn operand(loc: Loc) -> String {
    match loc {
        Loc::Reg(r) => r.to_string(),
        Loc::Spill(off) => format!("{off}(%rbp)"),
    }
}

pub(super) fn emit(ir: FunctionIr) -> String {
    let FunctionIr {
        name,
        module,
        nparams,
        vregs,
        floats,
        insts,
    } = ir;
    let ivs = intervals(&insts, vregs);
    let save_base = -8 * CALLEE_SAVED.len() as i64;
    let (loc, used_callee, spill_floor) = allocate(&ivs, &floats, save_base);
    // Frame temps (multi-word storage) sit below the spills; one static
    // slot per Temp site, reused across loop iterations.
    let mut temp_floor = spill_floor;
    let mut temp_off: HashMap<usize, i64> = HashMap::new();
    for (i, inst) in insts.iter().enumerate() {
        if let Inst::Temp { words, .. } = inst {
            temp_floor -= 8 * *words as i64;
            temp_off.insert(i, temp_floor);
        }
    }
    // Outgoing-args area (ADR 0024): the max stack-slot count over the
    // function's calls, reserved at the frame bottom — %rsp never moves,
    // so the alignment invariant holds at every call.
    let outgoing = insts
        .iter()
        .map(|inst| match inst {
            Inst::Call { args, sret, .. } => {
                (args.len() + sret.is_some() as usize).saturating_sub(6)
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let at = |v: V| operand(*loc.get(&v).expect("allocated"));

    let mut a = String::new();
    let label = label_of(module, &name);
    if label == syntax::ENTRY_FN {
        let _ = writeln!(a, "\t.globl {label}");
    }
    let _ = writeln!(a, "{label}:\n\tpushq %rbp\n\tmovq %rsp, %rbp");
    // One frame covers saves, spills, temps, and outgoing args, 16-byte
    // aligned; this backend never pushes operands, so %rsp stays aligned
    // at every call with no fix-ups.
    let frame = ((-temp_floor + 8 * outgoing as i64) + 15) & !15;
    if frame > 0 {
        let _ = writeln!(a, "\tsubq ${frame}, %rsp");
    }
    for (i, r) in used_callee.iter().enumerate() {
        let _ = writeln!(a, "\tmovq {r}, {}(%rbp)", -8 * (i as i64 + 1));
    }
    let restore = |a: &mut String| {
        for (i, r) in used_callee.iter().enumerate() {
            let _ = writeln!(a, "\tmovq {}(%rbp), {r}", -8 * (i as i64 + 1));
        }
    };
    // Params land in vregs 0..nparams (sret pointer included) in
    // argument-register order; slots 6+ read from above the frame
    // (ADR 0024). Param intervals start at instruction 0 — no defining
    // instruction, so liveness reaches entry — making these prologue
    // writes safe from later clobbers.
    for i in 0..nparams {
        let Some(l) = loc.get(&i) else { continue };
        match ARG_REGS.get(i) {
            Some(reg) => {
                let _ = writeln!(a, "\tmovq {reg}, {}", operand(*l));
            }
            None => {
                let off = 16 + 8 * (i as i64 - 6);
                if let Loc::Reg(r) = l {
                    let _ = writeln!(a, "\tmovq {off}(%rbp), {r}");
                } else {
                    let _ = writeln!(a, "\tmovq {off}(%rbp), %rax\n\tmovq %rax, {}", operand(*l));
                }
            }
        }
    }

    let mut bounds = 0usize;
    for (idx, inst) in insts.iter().enumerate() {
        match inst {
            Inst::Const(d, n) => match loc.get(d) {
                Some(Loc::Reg(r)) if !r.starts_with("%x") => {
                    let _ = writeln!(a, "\tmovabsq ${n}, {r}");
                }
                Some(l) => {
                    let _ = writeln!(a, "\tmovabsq ${n}, %rax\n\tmovq %rax, {}", operand(*l));
                }
                None => {}
            },
            Inst::Copy(d, s) => {
                let (ds, ss) = (at(*d), at(*s));
                if ds == ss {
                } else if matches!(loc[d], Loc::Reg(_)) || matches!(loc[s], Loc::Reg(_)) {
                    let _ = writeln!(a, "\tmovq {ss}, {ds}");
                } else {
                    let _ = writeln!(a, "\tmovq {ss}, %rax\n\tmovq %rax, {ds}");
                }
            }
            Inst::Temp { dst, .. } => {
                let off = temp_off[&idx];
                if let Some(Loc::Reg(r)) = loc.get(dst) {
                    let _ = writeln!(a, "\tleaq {off}(%rbp), {r}");
                } else if let Some(l) = loc.get(dst) {
                    let _ = writeln!(a, "\tleaq {off}(%rbp), %rax\n\tmovq %rax, {}", operand(*l));
                }
            }
            Inst::CopyW { dst, src, words } => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rdi\n\tmovq {}, %rsi\n\tmovq ${words}, %rcx\n\trep movsq",
                    at(*dst),
                    at(*src)
                );
            }
            Inst::LoadAt { dst, base, off } => {
                let _ = writeln!(a, "\tmovq {}, %rax\n\tmovq {off}(%rax), %rax", at(*base));
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::StoreAt { base, off, val } => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovq {}, %rcx\n\tmovq %rcx, {off}(%rax)",
                    at(*base),
                    at(*val)
                );
            }
            Inst::LeaAt { dst, base, off } => {
                let _ = writeln!(a, "\tmovq {}, %rax\n\tleaq {off}(%rax), %rax", at(*base));
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::LeaSym { dst, sym } => {
                if let Some(Loc::Reg(r)) = loc.get(dst)
                    && !r.starts_with("%x")
                {
                    let _ = writeln!(a, "\tleaq {sym}(%rip), {r}");
                    continue;
                }
                let _ = writeln!(a, "\tleaq {sym}(%rip), %rax\n\tmovq %rax, {}", at(*dst));
            }
            Inst::Neg(d, s) => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tnegq %rax\n\tmovq %rax, {}",
                    at(*s),
                    at(*d)
                );
            }
            Inst::NegF(d, s) => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tbtcq $63, %rax\n\tmovq %rax, {}",
                    at(*s),
                    at(*d)
                );
            }
            Inst::Not(d, s) => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\txorq $1, %rax\n\tmovq %rax, {}",
                    at(*s),
                    at(*d)
                );
            }
            Inst::BinImm { op, dst, lhs, imm } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul => {
                    if let Loc::Reg(d) = loc[dst] {
                        if matches!(op, BinOp::Mul) {
                            let _ = writeln!(a, "\timulq ${imm}, {}, {d}", at(*lhs));
                        } else {
                            if at(*lhs) != at(*dst) {
                                let _ = writeln!(a, "\tmovq {}, {d}", at(*lhs));
                            }
                            let mnem = if matches!(op, BinOp::Add) {
                                "addq"
                            } else {
                                "subq"
                            };
                            let _ = writeln!(a, "\t{mnem} ${imm}, {d}");
                        }
                    } else {
                        let _ = writeln!(a, "\tmovq {}, %rax", at(*lhs));
                        let mnem = match op {
                            BinOp::Add => "addq",
                            BinOp::Sub => "subq",
                            _ => "imulq",
                        };
                        let _ = writeln!(a, "\t{mnem} ${imm}, %rax\n\tmovq %rax, {}", at(*dst));
                    }
                }
                _ => {
                    let _ = writeln!(
                        a,
                        "\tcmpq ${imm}, {}\n\tset{} %al\n\tmovzbq %al, %rax\n\tmovq %rax, {}",
                        at(*lhs),
                        cc(*op),
                        at(*dst)
                    );
                }
            },
            Inst::DivPow2 { dst, src, k } => {
                let bias = (1i64 << k) - 1;
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tleaq {bias}(%rax), %rcx\n\ttestq %rax, %rax\n\
                     \tcmovnsq %rax, %rcx\n\tsarq ${k}, %rcx\n\tmovq %rcx, {}",
                    at(*src),
                    at(*dst)
                );
            }
            Inst::RemPow2 { dst, src, k } => {
                let bias = (1i64 << k) - 1;
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tleaq {bias}(%rax), %rcx\n\ttestq %rax, %rax\n\
                     \tcmovnsq %rax, %rcx\n\tsarq ${k}, %rcx\n\tshlq ${k}, %rcx\n\
                     \tsubq %rcx, %rax\n\tmovq %rax, {}",
                    at(*src),
                    at(*dst)
                );
            }
            Inst::DivChecked {
                dst,
                lhs,
                rhs,
                rem,
                loc,
            } => {
                bounds += 1;
                let ok0 = format!(".LTB{module}_{name}_{bounds}");
                bounds += 1;
                let ok1 = format!(".LTB{module}_{name}_{bounds}");
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rcx\n\ttestq %rcx, %rcx\n\tjne {ok0}\n\
                     \tleaq {loc}(%rip), %rdi\n\tcall {TRAP_DIV0}\n{ok0}:",
                    at(*rhs)
                );
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tcmpq $-1, %rcx\n\tjne {ok1}\n\
                     \tmovabsq $-9223372036854775808, %rdx\n\tcmpq %rdx, %rax\n\tjne {ok1}\n\
                     \tleaq {loc}(%rip), %rdi\n\tcall {TRAP_OVERFLOW}\n{ok1}:",
                    at(*lhs)
                );
                a.push_str("\tcqto\n\tidivq %rcx\n");
                if *rem {
                    a.push_str("\tmovq %rdx, %rax\n");
                }
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::DivMagic { dst, src, d } | Inst::RemMagic { dst, src, d } => {
                let (m, shift) = magic_i64(*d);
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovabsq ${m}, %rcx\n\timulq %rcx",
                    at(*src)
                );
                if m < 0 {
                    let _ = writeln!(a, "\taddq {}, %rdx", at(*src));
                }
                if shift > 0 {
                    let _ = writeln!(a, "\tsarq ${shift}, %rdx");
                }
                a.push_str("\tmovq %rdx, %rcx\n\tshrq $63, %rcx\n\taddq %rcx, %rdx\n");
                if matches!(inst, Inst::RemMagic { .. }) {
                    let _ = writeln!(
                        a,
                        "\tmovabsq ${d}, %rcx\n\timulq %rcx, %rdx\n\tmovq {}, %rax\n\
                         \tsubq %rdx, %rax\n\tmovq %rax, {}",
                        at(*src),
                        at(*dst)
                    );
                } else {
                    let _ = writeln!(a, "\tmovq %rdx, {}", at(*dst));
                }
            }
            Inst::Bin {
                op,
                float: false,
                dst,
                lhs,
                rhs,
            } => {
                if let (Loc::Reg(d), true) = (loc[dst], at(*dst) != at(*rhs))
                    && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                {
                    if at(*lhs) != at(*dst) {
                        let _ = writeln!(a, "\tmovq {}, {d}", at(*lhs));
                    }
                    let mnem = match op {
                        BinOp::Add => "addq",
                        BinOp::Sub => "subq",
                        _ => "imulq",
                    };
                    let _ = writeln!(a, "\t{mnem} {}, {d}", at(*rhs));
                    continue;
                }
                let _ = writeln!(a, "\tmovq {}, %rax", at(*lhs));
                match op {
                    BinOp::Add => {
                        let _ = writeln!(a, "\taddq {}, %rax", at(*rhs));
                    }
                    BinOp::Sub => {
                        let _ = writeln!(a, "\tsubq {}, %rax", at(*rhs));
                    }
                    BinOp::Mul => {
                        let _ = writeln!(a, "\timulq {}, %rax", at(*rhs));
                    }
                    BinOp::Div | BinOp::Rem => {
                        unreachable!("integer division lowers to DivChecked (ADR 0022)")
                    }
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let _ = writeln!(
                            a,
                            "\tcmpq {}, %rax\n\tset{} %al\n\tmovzbq %al, %rax",
                            at(*rhs),
                            cc(*op)
                        );
                    }
                    BinOp::And | BinOp::Or | BinOp::Coalesce => {
                        unreachable!("lowered to control flow")
                    }
                }
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::Bin {
                op,
                float: true,
                dst,
                lhs,
                rhs,
            } => {
                let _ = writeln!(a, "\tmovq {}, %xmm0\n\tmovq {}, %xmm1", at(*lhs), at(*rhs));
                let (code, from) = match op {
                    BinOp::Add => ("\taddsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Sub => ("\tsubsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Mul => ("\tmulsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Div => ("\tdivsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Rem => (format!("\tcall {RT_FMOD}\n"), "%xmm0"),
                    BinOp::Eq => ("\tcmpeqsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Ne => ("\tcmpneqsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Lt => ("\tcmpltsd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Le => ("\tcmplesd %xmm1, %xmm0\n".to_string(), "%xmm0"),
                    BinOp::Gt => ("\tcmpltsd %xmm0, %xmm1\n".to_string(), "%xmm1"),
                    BinOp::Ge => ("\tcmplesd %xmm0, %xmm1\n".to_string(), "%xmm1"),
                    BinOp::And | BinOp::Or | BinOp::Coalesce => {
                        unreachable!("lowered to control flow")
                    }
                };
                a.push_str(&code);
                let cmp = matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                );
                if let (Loc::Reg(d), false) = (loc[dst], cmp) {
                    let _ = writeln!(a, "\tmovq {from}, {d}");
                } else {
                    let _ = writeln!(a, "\tmovq {from}, %rax");
                    if cmp {
                        a.push_str("\tandq $1, %rax\n");
                    }
                    let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
                }
            }
            Inst::Call {
                dst,
                label,
                args,
                sret,
            } => {
                let shift = sret.is_some() as usize;
                if let Some(s) = sret {
                    let _ = writeln!(a, "\tmovq {}, %rdi", at(*s));
                }
                for (i, v) in args.iter().enumerate() {
                    match ARG_REGS.get(i + shift) {
                        Some(reg) => {
                            let _ = writeln!(a, "\tmovq {}, {reg}", at(*v));
                        }
                        // Slots 6+ store into the outgoing area, one
                        // block right before the call (ADR 0024).
                        None => {
                            let off = 8 * (i + shift - 6);
                            let _ =
                                writeln!(a, "\tmovq {}, %rax\n\tmovq %rax, {off}(%rsp)", at(*v));
                        }
                    }
                }
                let _ = writeln!(a, "\tcall {label}");
                if let Some(l) = loc.get(dst) {
                    let _ = writeln!(a, "\tmovq %rax, {}", operand(*l));
                }
            }
            Inst::CallRt {
                dst,
                sym,
                args,
                varargs,
            } => {
                for (i, v) in args.iter().enumerate() {
                    let _ = writeln!(a, "\tmovq {}, {}", at(*v), ARG_REGS[i]);
                }
                if *varargs {
                    a.push_str("\txorl %eax, %eax\n");
                }
                let _ = writeln!(a, "\tcall {sym}");
                if let Some(l) = loc.get(dst) {
                    let _ = writeln!(a, "\tmovq %rax, {}", operand(*l));
                }
            }
            Inst::StoreHdr { hdr, buf, len } => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovq {}, %rcx\n\tmovq %rcx, 16(%rax)\n\
                     \tmovq ${len}, 0(%rax)\n\tmovq ${len}, 8(%rax)",
                    at(*hdr),
                    at(*buf)
                );
            }
            Inst::BufSet { buf, slot, val } => {
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovq {}, %rcx\n\tmovq %rcx, {}(%rax)",
                    at(*buf),
                    at(*val),
                    8 * slot
                );
            }
            Inst::Len(d, arr) => {
                let _ = writeln!(a, "\tmovq {}, %rax\n\tmovq 0(%rax), %rax", at(*arr));
                let _ = writeln!(a, "\tmovq %rax, {}", at(*d));
            }
            Inst::Index {
                dst,
                arr,
                idx,
                loc,
                agg,
            } => {
                bounds += 1;
                let target = format!(".LTB{module}_{name}_{bounds}");
                a.push_str(&bounds_check(&at(*arr), &at(*idx), loc, &target));
                match agg {
                    None => {
                        a.push_str("\tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax\n");
                    }
                    // Interior pointer: data + idx*stride (ADR 0023).
                    Some(words) => {
                        let _ = writeln!(
                            a,
                            "\tmovq 16(%rax), %rax\n\timulq ${}, %rcx, %rcx\n\taddq %rcx, %rax",
                            8 * *words
                        );
                    }
                }
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::IndexSet {
                arr,
                idx,
                val,
                loc,
                agg,
            } => {
                bounds += 1;
                let target = format!(".LTB{module}_{name}_{bounds}");
                a.push_str(&bounds_check(&at(*arr), &at(*idx), loc, &target));
                match agg {
                    None => {
                        let _ = writeln!(
                            a,
                            "\tmovq 16(%rax), %rax\n\tmovq {}, %rdx\n\tmovq %rdx, (%rax,%rcx,8)",
                            at(*val)
                        );
                    }
                    // rep movsq from the value pointer into the buffer
                    // slot — %rdi/%rsi/%rcx are reserved scratch (CopyW).
                    Some(words) => {
                        let _ = writeln!(
                            a,
                            "\tmovq 16(%rax), %rdi\n\timulq ${}, %rcx, %rcx\n\taddq %rcx, %rdi\n\
                             \tmovq {}, %rsi\n\tmovq ${words}, %rcx\n\trep movsq",
                            8 * *words,
                            at(*val)
                        );
                    }
                }
            }
            Inst::Ret(v) => {
                let _ = writeln!(a, "\tmovq {}, %rax", at(*v));
                restore(&mut a);
                a.push_str("\tleave\n\tret\n");
            }
            Inst::Jmp(l) => {
                let _ = writeln!(a, "\tjmp .LT{module}_{name}_{l}");
            }
            Inst::BrZero(v, l) => {
                if let Loc::Reg(r) = loc[v] {
                    let _ = writeln!(a, "\ttestq {r}, {r}\n\tje .LT{module}_{name}_{l}");
                } else {
                    let _ = writeln!(a, "\tcmpq $0, {}\n\tje .LT{module}_{name}_{l}", at(*v));
                }
            }
            Inst::Label(l) => {
                let _ = writeln!(a, ".LT{module}_{name}_{l}:");
            }
        }
    }
    a
}
