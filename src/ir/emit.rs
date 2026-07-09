//! IR → AT&T assembly, one small block per instruction. %rax/%rcx/%rdx
//! (and %xmm0/%xmm1) are permanent scratch — never allocated — so every
//! sequence may use them freely. The frame is laid out as
//! [saved callee regs | spills | multi-word temps], 16-byte aligned;
//! this backend never pushes operands, so %rsp stays aligned at every
//! call site with no fix-ups.

use super::lower::Lowerer;
use super::regalloc::{allocate, intervals, Loc, ARG_REGS, CALLEE_SAVED};
use super::{cc, Inst, V};
use crate::ast::BinOp;
use crate::codegen::label_of;
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

fn operand(loc: Loc) -> String {
    match loc {
        Loc::Reg(r) => r.to_string(),
        Loc::Spill(off) => format!("{off}(%rbp)"),
    }
}

pub(super) fn emit(name: &str, module: usize, nparams: usize, lo: Lowerer) -> String {
    let ivs = intervals(&lo.insts, lo.vregs);
    let save_base = -8 * CALLEE_SAVED.len() as i64;
    let (loc, used_callee, spill_floor) = allocate(&ivs, &lo.floats, save_base);
    // Frame temps (multi-word storage) sit below the spills; one static
    // slot per Temp site, reused across loop iterations.
    let mut temp_floor = spill_floor;
    let mut temp_off: HashMap<usize, i64> = HashMap::new();
    for (i, inst) in lo.insts.iter().enumerate() {
        if let Inst::Temp { words, .. } = inst {
            temp_floor -= 8 * *words as i64;
            temp_off.insert(i, temp_floor);
        }
    }
    let at = |v: V| operand(*loc.get(&v).expect("allocated"));

    let mut a = String::new();
    let label = label_of(module, name);
    if label == "main" {
        a.push_str("\t.globl main\n");
    }
    let _ = writeln!(a, "{label}:\n\tpushq %rbp\n\tmovq %rsp, %rbp");
    // One frame covers saves, spills, and temps, 16-byte aligned; this
    // backend never pushes operands, so %rsp stays aligned at every call
    // with no fix-ups.
    let frame = ((-temp_floor) + 15) & !15;
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
    // argument-register order.
    for (i, reg) in ARG_REGS.iter().take(nparams).enumerate() {
        if let Some(l) = loc.get(&i) {
            let _ = writeln!(a, "\tmovq {reg}, {}", operand(*l));
        }
    }

    let mut bounds = 0usize;
    for (idx, inst) in lo.insts.iter().enumerate() {
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
                if let Some(Loc::Reg(r)) = loc.get(dst) {
                    if !r.starts_with("%x") {
                        let _ = writeln!(a, "\tleaq {sym}(%rip), {r}");
                        continue;
                    }
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
                if let (Loc::Reg(d), true) = (loc[dst], at(*dst) != at(*rhs)) {
                    if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul) {
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
                        let _ = writeln!(a, "\tmovq {}, %rcx\n\tcqto\n\tidivq %rcx", at(*rhs));
                        if matches!(op, BinOp::Rem) {
                            a.push_str("\tmovq %rdx, %rax\n");
                        }
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
                    BinOp::Add => ("\taddsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Sub => ("\tsubsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Mul => ("\tmulsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Div => ("\tdivsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Rem => ("\tcall fmod@PLT\n", "%xmm0"),
                    BinOp::Eq => ("\tcmpeqsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Ne => ("\tcmpneqsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Lt => ("\tcmpltsd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Le => ("\tcmplesd %xmm1, %xmm0\n", "%xmm0"),
                    BinOp::Gt => ("\tcmpltsd %xmm0, %xmm1\n", "%xmm1"),
                    BinOp::Ge => ("\tcmplesd %xmm0, %xmm1\n", "%xmm1"),
                    BinOp::And | BinOp::Or | BinOp::Coalesce => {
                        unreachable!("lowered to control flow")
                    }
                };
                a.push_str(code);
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
                    let _ = writeln!(a, "\tmovq {}, {}", at(*v), ARG_REGS[i + shift]);
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
            Inst::Index { dst, arr, idx } => {
                bounds += 1;
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovq {}, %rcx\n\tcmpq 0(%rax), %rcx\n\
                     \tjb .LTB{module}_{name}_{bounds}\n\tcall abort@PLT\n\
                     .LTB{module}_{name}_{bounds}:\n\
                     \tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax",
                    at(*arr),
                    at(*idx)
                );
                let _ = writeln!(a, "\tmovq %rax, {}", at(*dst));
            }
            Inst::IndexSet { arr, idx, val } => {
                bounds += 1;
                let _ = writeln!(
                    a,
                    "\tmovq {}, %rax\n\tmovq {}, %rcx\n\tcmpq 0(%rax), %rcx\n\
                     \tjb .LTB{module}_{name}_{bounds}\n\tcall abort@PLT\n\
                     .LTB{module}_{name}_{bounds}:\n\
                     \tmovq 16(%rax), %rax\n\tmovq {}, %rdx\n\tmovq %rdx, (%rax,%rcx,8)",
                    at(*arr),
                    at(*idx),
                    at(*val)
                );
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
