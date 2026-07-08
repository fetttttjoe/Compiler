//! The optimizing tier (ADR 0016): a flat virtual-register IR with
//! linear-scan register allocation. `lower` returns `None` for any
//! function using constructs this tier doesn't cover yet — the caller
//! falls back to the direct emitter, so coverage grows per function
//! without ever risking correctness. Locals are mutable vregs (no phis);
//! SSA arrives with the GVN slice, per the ADR.

use crate::ast::{BinOp, Expr, Function, Stmt, UnOp};
use crate::check::Resolutions;
use crate::span::Span;
use crate::types::Type;
use std::collections::HashMap;
use std::fmt::Write;

/// A virtual register.
type V = usize;
type Lbl = usize;

enum Inst {
    Const(V, i64),
    Copy(V, V),
    /// dst, lhs, rhs; `float` selects the SSE path.
    Bin {
        op: BinOp,
        float: bool,
        dst: V,
        lhs: V,
        rhs: V,
    },
    Neg(V, V),
    NegF(V, V),
    Not(V, V),
    Call {
        dst: V,
        label: String,
        args: Vec<V>,
    },
    Ret(V),
    Jmp(Lbl),
    /// Falls through when `cond` is nonzero, jumps when zero.
    BrZero(V, Lbl),
    Label(Lbl),
}

/// Everything word-typed the first tier accepts.
fn word_scalar(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Bool | Type::Float)
}

struct Lowerer<'a> {
    res: &'a Resolutions,
    module: usize,
    insts: Vec<Inst>,
    scopes: Vec<HashMap<String, V>>,
    vregs: usize,
    /// Parallel to vregs: floats allocate from the XMM pool.
    floats: Vec<bool>,
    labels: usize,
}

/// Lowers `f` if every construct fits the first tier; `None` = fall back.
pub fn lower(f: &Function, module: usize, res: &Resolutions) -> Option<String> {
    // Signature gate: word scalars only, and few enough for registers.
    let sig = res.sigs.get(&(module, f.name.clone()))?;
    if !sig.params.iter().all(word_scalar) {
        return None;
    }
    match &sig.ret {
        Type::Unit => {}
        t if word_scalar(t) => {}
        _ => return None,
    }
    if f.params.len() > 6 {
        return None;
    }

    let mut lo = Lowerer {
        res,
        module,
        insts: Vec::new(),
        scopes: vec![HashMap::new()],
        vregs: 0,
        floats: Vec::new(),
        labels: 0,
    };
    // Params land in fresh vregs; emission moves them from the arg regs.
    for (p, ty) in f.params.iter().zip(&sig.params) {
        let v = lo.fresh(*ty == Type::Float);
        lo.scopes[0].insert(p.name.clone(), v);
    }
    for stmt in &f.body {
        lo.stmt(stmt)?;
    }
    // Fall-through for unit functions; value functions always return
    // (checker-proven) so the extra ret is dead.
    let zero = lo.fresh(false);
    lo.insts.push(Inst::Const(zero, 0));
    lo.insts.push(Inst::Ret(zero));

    // Pathologically large functions (machine-generated operator chains)
    // would make the O(insts × vregs) liveness pass crawl; the direct
    // tier compiles them fine.
    if lo.vregs > 2_000 {
        return None;
    }

    Some(emit(&f.name, module, f.params.len(), lo))
}

impl Lowerer<'_> {
    fn fresh(&mut self, float: bool) -> V {
        self.vregs += 1;
        self.floats.push(float);
        self.vregs - 1
    }

    fn fresh_label(&mut self) -> Lbl {
        self.labels += 1;
        self.labels - 1
    }

    fn lookup(&self, name: &str) -> Option<V> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn is_float(&self, e: &Expr) -> bool {
        matches!(self.res.expr_types.get(&e.span()), Some(Type::Float))
    }

    fn word_expr(&self, span: &Span) -> bool {
        self.res.expr_types.get(span).is_some_and(word_scalar)
    }

    fn block(&mut self, body: &[Stmt]) -> Option<()> {
        self.scopes.push(HashMap::new());
        let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
        self.scopes.pop();
        result
    }

    fn stmt(&mut self, stmt: &Stmt) -> Option<()> {
        match stmt {
            Stmt::Let { name, value, .. } => {
                if !self.word_expr(&value.span()) {
                    return None;
                }
                let v = self.expr(value)?;
                // A fresh vreg per binding keeps shadowing exact.
                let slot = self.fresh(self.floats[v]);
                self.insts.push(Inst::Copy(slot, v));
                self.scopes
                    .last_mut()
                    .expect("a scope is always open")
                    .insert(name.clone(), slot);
            }
            Stmt::Assign { target, value, .. } => {
                let Expr::Ident(name, _) = target else {
                    return None;
                };
                let dst = self.lookup(name)?;
                let v = self.expr(value)?;
                self.insts.push(Inst::Copy(dst, v));
            }
            Stmt::Return { value, .. } => match value {
                Some(expr) => {
                    let v = self.expr(expr)?;
                    self.insts.push(Inst::Ret(v));
                }
                None => {
                    let zero = self.fresh(false);
                    self.insts.push(Inst::Const(zero, 0));
                    self.insts.push(Inst::Ret(zero));
                }
            },
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                let c = self.expr(cond)?;
                let end = self.fresh_label();
                match else_body {
                    None => {
                        self.insts.push(Inst::BrZero(c, end));
                        self.block(then_body)?;
                    }
                    Some(else_body) => {
                        let otherwise = self.fresh_label();
                        self.insts.push(Inst::BrZero(c, otherwise));
                        self.block(then_body)?;
                        self.insts.push(Inst::Jmp(end));
                        self.insts.push(Inst::Label(otherwise));
                        self.block(else_body)?;
                    }
                }
                self.insts.push(Inst::Label(end));
            }
            Stmt::While { cond, body, .. } => {
                let top = self.fresh_label();
                let end = self.fresh_label();
                self.insts.push(Inst::Label(top));
                let c = self.expr(cond)?;
                self.insts.push(Inst::BrZero(c, end));
                self.block(body)?;
                self.insts.push(Inst::Jmp(top));
                self.insts.push(Inst::Label(end));
            }
            Stmt::Expr(expr) => {
                self.expr(expr)?;
            }
            Stmt::For { .. } => return None,
        }
        Some(())
    }

    fn expr(&mut self, expr: &Expr) -> Option<V> {
        match expr {
            Expr::Int(n, _) => {
                let v = self.fresh(false);
                self.insts.push(Inst::Const(v, *n));
                Some(v)
            }
            Expr::Bool(b, _) => {
                let v = self.fresh(false);
                self.insts.push(Inst::Const(v, *b as i64));
                Some(v)
            }
            Expr::Float(f, _) => {
                let v = self.fresh(true);
                self.insts.push(Inst::Const(v, f.to_bits() as i64));
                Some(v)
            }
            Expr::Ident(name, _) => self.lookup(name),
            Expr::Unary { op, rhs, .. } => {
                let float = self.is_float(rhs);
                let r = self.expr(rhs)?;
                let v = self.fresh(float);
                self.insts.push(match op {
                    UnOp::Neg if float => Inst::NegF(v, r),
                    UnOp::Neg => Inst::Neg(v, r),
                    UnOp::Not => Inst::Not(v, r),
                });
                Some(v)
            }
            Expr::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
                ..
            } => {
                // Mutable vregs make short-circuit trivial: the result
                // starts as the left side and is overwritten only when
                // the right side runs.
                let v = self.fresh(false);
                let l = self.expr(lhs)?;
                self.insts.push(Inst::Copy(v, l));
                let end = self.fresh_label();
                if matches!(op, BinOp::And) {
                    self.insts.push(Inst::BrZero(v, end));
                } else {
                    // Or: skip the rhs when v is nonzero — invert with a
                    // Not into a scratch and branch on that.
                    let inv = self.fresh(false);
                    self.insts.push(Inst::Not(inv, v));
                    self.insts.push(Inst::BrZero(inv, end));
                }
                let r = self.expr(rhs)?;
                self.insts.push(Inst::Copy(v, r));
                self.insts.push(Inst::Label(end));
                Some(v)
            }
            Expr::Binary {
                op: BinOp::Coalesce,
                ..
            } => None,
            Expr::Binary { op, lhs, rhs, .. } => {
                if !self.word_expr(&lhs.span()) {
                    return None;
                }
                let float = self.is_float(lhs);
                let l = self.expr(lhs)?;
                let r = self.expr(rhs)?;
                // Comparisons yield bools; only arithmetic stays float.
                let arith = !matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                );
                let v = self.fresh(float && arith);
                self.insts.push(Inst::Bin {
                    op: *op,
                    float,
                    dst: v,
                    lhs: l,
                    rhs: r,
                });
                Some(v)
            }
            Expr::Call { callee, args, span } => {
                if !self
                    .res
                    .expr_types
                    .get(span)
                    .is_some_and(|t| word_scalar(t) || *t == Type::Unit)
                {
                    return None;
                }
                let Expr::Ident(name, _) = callee.as_ref() else {
                    return None;
                };
                // Builtins (print/len/push) belong to the direct tier.
                let key = self.res.functions[self.module].get(name)?;
                let sig = self.res.sigs.get(key)?;
                if !sig.params.iter().all(word_scalar) || args.len() > 6 {
                    return None;
                }
                let ret_float = sig.ret == Type::Float;
                let args: Vec<V> = args.iter().map(|a| self.expr(a)).collect::<Option<_>>()?;
                let dst = self.fresh(ret_float);
                self.insts.push(Inst::Call {
                    dst,
                    label: crate::codegen::label_of(key.0, &key.1),
                    args,
                });
                Some(dst)
            }
            _ => None,
        }
    }
}

// ---- Register allocation -----------------------------------------------

/// Intervals crossing a call must survive the call.
const CALLEE_SAVED: [&str; 5] = ["%rbx", "%r12", "%r13", "%r14", "%r15"];
/// Cheap registers for call-free intervals; never argument registers, so
/// call setup can't clobber a live value.
const CALLER_SAVED: [&str; 2] = ["%r10", "%r11"];
/// Float pool. All XMM registers are caller-saved, so call-crossing
/// float intervals spill (%xmm0/%xmm1 stay operation scratch).
const XMM_POOL: [&str; 12] = [
    "%xmm2", "%xmm3", "%xmm4", "%xmm5", "%xmm6", "%xmm7", "%xmm8", "%xmm9", "%xmm10", "%xmm11",
    "%xmm12", "%xmm13",
];
const ARG_REGS: [&str; 6] = ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"];

#[derive(Clone, Copy, PartialEq)]
enum Loc {
    Reg(&'static str),
    /// rbp-relative offset.
    Spill(i64),
}

struct Interval {
    vreg: V,
    start: usize,
    end: usize,
    crosses_call: bool,
}

/// Live intervals by iterative backward dataflow over the flat list.
fn intervals(insts: &[Inst], vregs: usize) -> Vec<Interval> {
    let mut label_pos = HashMap::new();
    for (i, inst) in insts.iter().enumerate() {
        if let Inst::Label(l) = inst {
            label_pos.insert(*l, i);
        }
    }
    let succs = |i: usize| -> Vec<usize> {
        match &insts[i] {
            Inst::Jmp(l) => vec![label_pos[l]],
            Inst::BrZero(_, l) => vec![i + 1, label_pos[l]],
            Inst::Ret(_) => vec![],
            _ if i + 1 < insts.len() => vec![i + 1],
            _ => vec![],
        }
    };
    let uses_defs = |inst: &Inst| -> (Vec<V>, Option<V>) {
        match inst {
            Inst::Const(d, _) => (vec![], Some(*d)),
            Inst::Copy(d, s) => (vec![*s], Some(*d)),
            Inst::Bin { dst, lhs, rhs, .. } => (vec![*lhs, *rhs], Some(*dst)),
            Inst::Neg(d, s) | Inst::NegF(d, s) | Inst::Not(d, s) => (vec![*s], Some(*d)),
            Inst::Call { dst, args, .. } => (args.clone(), Some(*dst)),
            Inst::Ret(v) => (vec![*v], None),
            Inst::BrZero(v, _) => (vec![*v], None),
            Inst::Jmp(_) | Inst::Label(_) => (vec![], None),
        }
    };

    let mut live_in: Vec<Vec<bool>> = vec![vec![false; vregs]; insts.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..insts.len()).rev() {
            let mut out = vec![false; vregs];
            for s in succs(i) {
                for (v, flag) in live_in[s].iter().enumerate() {
                    if *flag {
                        out[v] = true;
                    }
                }
            }
            let (uses, def) = uses_defs(&insts[i]);
            if let Some(d) = def {
                out[d] = false;
            }
            for u in uses {
                out[u] = true;
            }
            if out != live_in[i] {
                live_in[i] = out;
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
    for (i, inst) in insts.iter().enumerate() {
        let (uses, def) = uses_defs(inst);
        for v in uses.into_iter().chain(def) {
            ivs[v].start = ivs[v].start.min(i);
            ivs[v].end = ivs[v].end.max(i);
        }
        for (v, flag) in live_in[i].iter().enumerate() {
            if *flag {
                ivs[v].start = ivs[v].start.min(i);
                ivs[v].end = ivs[v].end.max(i);
            }
        }
    }
    for (i, inst) in insts.iter().enumerate() {
        let is_call = matches!(inst, Inst::Call { .. })
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
                // Live around the call site (not merely defined by it).
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
/// intervals, caller-saved GPRs otherwise, XMM for floats (all XMM are
/// caller-saved, so call-crossing floats spill); losers spill below the
/// callee-saved save area.
fn allocate(
    ivs: &[Interval],
    floats: &[bool],
    spill_base: i64,
) -> (HashMap<V, Loc>, Vec<&'static str>, i64) {
    let mut free_callee: Vec<&'static str> = CALLEE_SAVED.to_vec();
    let mut free_caller: Vec<&'static str> = CALLER_SAVED.to_vec();
    let mut free_xmm: Vec<&'static str> = XMM_POOL.to_vec();
    let mut active: Vec<(usize, V)> = Vec::new(); // (end, vreg)
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

// ---- Emission ------------------------------------------------------------

fn operand(loc: Loc) -> String {
    match loc {
        Loc::Reg(r) => r.to_string(),
        Loc::Spill(off) => format!("{off}(%rbp)"),
    }
}

fn emit(name: &str, module: usize, nparams: usize, lo: Lowerer) -> String {
    let ivs = intervals(&lo.insts, lo.vregs);
    // Callee-saved registers save right below %rbp; spills below them.
    let save_base = -8 * CALLEE_SAVED.len() as i64;
    let (loc, used_callee, spill_floor) = allocate(&ivs, &lo.floats, save_base);
    let at = |v: V| operand(*loc.get(&v).expect("allocated"));

    let mut a = String::new();
    let label = crate::codegen::label_of(module, name);
    if label == "main" {
        a.push_str("\t.globl main\n");
    }
    let _ = writeln!(a, "{label}:\n\tpushq %rbp\n\tmovq %rsp, %rbp");
    // One frame covers the save area and every spill, 16-byte aligned;
    // no operand pushes exist in this tier, so %rsp stays aligned at
    // every call site with no fix-ups.
    let frame = ((-spill_floor) + 15) & !15;
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
    // Params: vregs 0..nparams by construction.
    for (i, reg) in ARG_REGS.iter().take(nparams).enumerate() {
        if let Some(l) = loc.get(&i) {
            let _ = writeln!(a, "\tmovq {reg}, {}", operand(*l));
        }
    }

    for inst in &lo.insts {
        match inst {
            Inst::Const(d, n) => {
                // movabsq targets a GPR directly; XMM and spill
                // destinations hop through %rax.
                match loc.get(d) {
                    Some(Loc::Reg(r)) if !r.starts_with("%x") => {
                        let _ = writeln!(a, "\tmovabsq ${n}, {r}");
                    }
                    Some(l) => {
                        let _ = writeln!(a, "\tmovabsq ${n}, %rax\n\tmovq %rax, {}", operand(*l));
                    }
                    None => {}
                }
            }
            Inst::Copy(d, s) => {
                let (ds, ss) = (at(*d), at(*s));
                if ds == ss {
                    // Self-copy: nothing to do.
                } else if matches!(loc[d], Loc::Reg(_)) || matches!(loc[s], Loc::Reg(_)) {
                    let _ = writeln!(a, "\tmovq {ss}, {ds}");
                } else {
                    let _ = writeln!(a, "\tmovq {ss}, %rax\n\tmovq %rax, {ds}");
                }
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
            Inst::Bin {
                op,
                float: false,
                dst,
                lhs,
                rhs,
            } => {
                // Commutative-free fast path: dst in a register that
                // doesn't alias rhs computes in place, no %rax hop.
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
                        let cc = match op {
                            BinOp::Eq => "e",
                            BinOp::Ne => "ne",
                            BinOp::Lt => "l",
                            BinOp::Le => "le",
                            BinOp::Gt => "g",
                            _ => "ge",
                        };
                        let _ = writeln!(
                            a,
                            "\tcmpq {}, %rax\n\tset{cc} %al\n\tmovzbq %al, %rax",
                            at(*rhs)
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
                // movq into xmm accepts a register or memory source
                // directly — no %rax staging needed.
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
            Inst::Call { dst, label, args } => {
                for (i, v) in args.iter().enumerate() {
                    let _ = writeln!(a, "\tmovq {}, {}", at(*v), ARG_REGS[i]);
                }
                let _ = writeln!(a, "\tcall {label}");
                if let Some(l) = loc.get(dst) {
                    let _ = writeln!(a, "\tmovq %rax, {}", operand(*l));
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
