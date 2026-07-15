//! Who gets a CPU register. Liveness (a bitset dataflow over the flat
//! instruction list) finds each virtual register's live interval, then
//! a linear scan hands out registers from three pools: callee-saved
//! GPRs for intervals that must survive a call, cheap caller-saved GPRs
//! otherwise, XMM for floats. No pool contains an argument register, so
//! call setup can never clobber a live value; the losers spill to frame
//! slots below the callee-saved save area.

use super::{Inst, V};
use crate::ast::BinOp;
use std::collections::HashMap;

/// Intervals crossing a call must survive the call.
pub(super) const CALLEE_SAVED: [&str; 5] = ["%rbx", "%r12", "%r13", "%r14", "%r15"];
/// Cheap registers for call-free intervals; never argument registers, so
/// call setup can't clobber a live value.
pub(super) const CALLER_SAVED: [&str; 2] = ["%r10", "%r11"];
/// Float pool. All XMM registers are caller-saved, so call-crossing
/// float intervals spill (%xmm0/%xmm1 stay operation scratch).
pub(super) const XMM_POOL: [&str; 12] = [
    "%xmm2", "%xmm3", "%xmm4", "%xmm5", "%xmm6", "%xmm7", "%xmm8", "%xmm9", "%xmm10", "%xmm11",
    "%xmm12", "%xmm13",
];
pub(super) const ARG_REGS: [&str; 6] = ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Loc {
    Reg(&'static str),
    Spill(i64),
}

pub(super) struct Interval {
    pub(super) vreg: V,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) crosses_call: bool,
}

pub(super) fn uses_defs(inst: &Inst) -> (Vec<V>, Option<V>) {
    match inst {
        Inst::Const(d, _) => (vec![], Some(*d)),
        Inst::Copy(d, s) => (vec![*s], Some(*d)),
        Inst::Bin { dst, lhs, rhs, .. } => (vec![*lhs, *rhs], Some(*dst)),
        Inst::BinImm { dst, lhs, .. } => (vec![*lhs], Some(*dst)),
        Inst::DivPow2 { dst, src, .. }
        | Inst::RemPow2 { dst, src, .. }
        | Inst::DivMagic { dst, src, .. }
        | Inst::RemMagic { dst, src, .. } => (vec![*src], Some(*dst)),
        // The trap stubs never return, so this is no call-clobber point.
        Inst::DivChecked { dst, lhs, rhs, .. } => (vec![*lhs, *rhs], Some(*dst)),
        Inst::Neg(d, s) | Inst::NegF(d, s) | Inst::Not(d, s) | Inst::IntToFloat(d, s) => {
            (vec![*s], Some(*d))
        }
        // Like DivChecked: the trap never returns, so no call-clobber.
        Inst::FloatToInt { dst, src, .. } => (vec![*src], Some(*dst)),
        Inst::Call {
            dst, args, sret, ..
        } => {
            let mut uses = args.clone();
            if let Some(s) = sret {
                uses.push(*s);
            }
            (uses, Some(*dst))
        }
        Inst::CallRt { dst, args, .. } => (args.clone(), Some(*dst)),
        Inst::Temp { dst, .. } => (vec![], Some(*dst)),
        Inst::CopyW { dst, src, .. } => (vec![*dst, *src], None),
        Inst::LoadAt { dst, base, .. } => (vec![*base], Some(*dst)),
        Inst::StoreAt { base, val, .. } => (vec![*base, *val], None),
        Inst::LeaAt { dst, base, .. } => (vec![*base], Some(*dst)),
        Inst::LeaSym { dst, .. } => (vec![], Some(*dst)),
        Inst::StoreHdr { hdr, buf, .. } => (vec![*hdr, *buf], None),
        Inst::BufSet { buf, val, .. } => (vec![*buf, *val], None),
        Inst::Len(d, arr) => (vec![*arr], Some(*d)),
        Inst::Index { dst, arr, idx, .. } => (vec![*arr, *idx], Some(*dst)),
        Inst::IndexSet { arr, idx, val, .. } => (vec![*arr, *idx, *val], None),
        Inst::Ret(v) => (vec![*v], None),
        Inst::BrZero(v, _) => (vec![*v], None),
        Inst::Jmp(_) | Inst::Label(_) => (vec![], None),
    }
}

/// Live intervals by iterative backward dataflow over the flat list —
/// bitset per instruction, so machine-generated functions stay fast.
pub(super) fn intervals(insts: &[Inst], vregs: usize) -> Vec<Interval> {
    let words = vregs.div_ceil(64);
    let mut label_pos = HashMap::new();
    for (i, inst) in insts.iter().enumerate() {
        if let Inst::Label(l) = inst {
            label_pos.insert(*l, i);
        }
    }
    // The instruction list is immutable during the fixpoint: resolve
    // uses/defs and successors once, not once per pass.
    let ud: Vec<(Vec<V>, Option<V>)> = insts.iter().map(uses_defs).collect();
    let succ: Vec<Vec<usize>> = insts
        .iter()
        .enumerate()
        .map(|(i, inst)| match inst {
            Inst::Jmp(l) => vec![label_pos[l]],
            Inst::BrZero(_, l) => vec![i + 1, label_pos[l]],
            Inst::Ret(_) => vec![],
            _ if i + 1 < insts.len() => vec![i + 1],
            _ => vec![],
        })
        .collect();

    let mut live_in: Vec<Vec<u64>> = vec![vec![0; words]; insts.len()];
    let mut out = vec![0u64; words];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..insts.len()).rev() {
            out.iter_mut().for_each(|w| *w = 0);
            for s in &succ[i] {
                for (o, w) in out.iter_mut().zip(&live_in[*s]) {
                    *o |= w;
                }
            }
            let (uses, def) = &ud[i];
            if let Some(d) = def {
                out[d / 64] &= !(1 << (d % 64));
            }
            for u in uses {
                out[u / 64] |= 1 << (u % 64);
            }
            if out != live_in[i] {
                live_in[i].copy_from_slice(&out);
                changed = true;
            }
        }
    }

    let mut ivs: Vec<Interval> = (0..vregs)
        .map(|v| Interval {
            vreg: v,
            start: usize::MAX,
            end: 0,
            crosses_call: false,
        })
        .collect();
    let touch = |v: usize, i: usize, ivs: &mut Vec<Interval>| {
        ivs[v].start = ivs[v].start.min(i);
        ivs[v].end = ivs[v].end.max(i);
    };
    for (i, (uses, def)) in ud.iter().enumerate() {
        for v in uses.iter().copied().chain(def.iter().copied()) {
            touch(v, i, &mut ivs);
        }
        for (w, word) in live_in[i].iter().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                touch(w * 64 + b, i, &mut ivs);
                bits &= bits - 1;
            }
        }
    }
    for (i, inst) in insts.iter().enumerate() {
        let is_call = matches!(inst, Inst::Call { .. } | Inst::CallRt { .. })
            || matches!(
                inst,
                Inst::Bin {
                    op: BinOp::Rem,
                    float: true,
                    ..
                }
            );
        if is_call {
            for iv in ivs.iter_mut() {
                if iv.start < i && iv.end > i {
                    iv.crosses_call = true;
                }
            }
        }
    }
    ivs.retain(|iv| iv.start != usize::MAX);
    ivs.sort_by_key(|iv| iv.start);
    ivs
}

/// Linear scan over three pools: callee-saved GPRs for call-crossing
/// intervals, caller-saved GPRs otherwise, XMM for floats; losers spill
/// below the callee-saved save area.
pub(super) fn allocate(
    ivs: &[Interval],
    floats: &[bool],
    spill_base: i64,
) -> (HashMap<V, Loc>, Vec<&'static str>, i64) {
    let mut free_callee: Vec<&'static str> = CALLEE_SAVED.to_vec();
    let mut free_caller: Vec<&'static str> = CALLER_SAVED.to_vec();
    let mut free_xmm: Vec<&'static str> = XMM_POOL.to_vec();
    let mut active: Vec<(usize, V)> = Vec::new();
    let mut loc: HashMap<V, Loc> = HashMap::new();
    let mut used_callee: Vec<&'static str> = Vec::new();
    let mut next_spill = spill_base;

    for iv in ivs {
        active.retain(|(end, v)| {
            if *end < iv.start {
                if let Some(Loc::Reg(r)) = loc.get(v) {
                    if r.starts_with("%x") {
                        free_xmm.push(r);
                    } else if CALLEE_SAVED.contains(r) {
                        free_callee.push(r);
                    } else {
                        free_caller.push(r);
                    }
                }
                false
            } else {
                true
            }
        });
        let reg = if floats[iv.vreg] {
            if iv.crosses_call {
                None
            } else {
                free_xmm.pop()
            }
        } else if iv.crosses_call {
            free_callee.pop()
        } else {
            free_caller.pop().or_else(|| free_callee.pop())
        };
        match reg {
            Some(r) => {
                if CALLEE_SAVED.contains(&r) && !used_callee.contains(&r) {
                    used_callee.push(r);
                }
                loc.insert(iv.vreg, Loc::Reg(r));
                active.push((iv.end, iv.vreg));
            }
            None => {
                next_spill -= 8;
                loc.insert(iv.vreg, Loc::Spill(next_spill));
            }
        }
    }
    (loc, used_callee, next_spill)
}
