//! AST → IR. One pass over the checked function body; every construct
//! becomes a handful of [`Inst`]s in the oracle's evaluation order.
//! Anything the backend can't represent yet returns a clean
//! "not yet compilable" diagnostic — there is no fallback path.

use super::layout::{FUEL, Kind, kind_of, offset_of, ref_shaped};
use super::{FunctionIr, Inst, Lbl, V, unsupported};
use crate::ast::{BinOp, Expr, Function, Stmt, UnOp};
use crate::check::Resolutions;
use crate::codegen::{Strings, label_of};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::types::Type;
use std::collections::HashMap;

pub(super) struct Lowerer<'a> {
    pub(super) res: &'a Resolutions,
    pub(super) strings: &'a mut Strings,
    pub(super) module: usize,
    pub(super) insts: Vec<Inst>,
    pub(super) scopes: Vec<HashMap<String, V>>,
    pub(super) vregs: usize,
    /// Parallel to vregs: floats allocate from the XMM pool.
    pub(super) floats: Vec<bool>,
    pub(super) labels: usize,
    /// Enclosing loops, innermost last: (continue target, break target).
    /// `for`'s continue target is its increment step, not the loop top —
    /// jumping to the top would re-run the same element (ADR 0019).
    pub(super) loops: Vec<(Lbl, Lbl)>,
    /// The hidden destination pointer of a struct-returning function.
    pub(super) sret: Option<V>,
    pub(super) ret_words: usize,
}

/// Lowers one checked function into owned virtual-register IR.
pub(super) fn lower(
    f: &Function,
    module: usize,
    res: &Resolutions,
    strings: &mut Strings,
) -> Result<FunctionIr, Diagnostic> {
    let sig = &res.sigs[&(module, f.name.clone())];
    let ret_kind = match &sig.ret {
        Type::Unit => Kind::Word,
        t => kind_of(t, res, FUEL).ok_or_else(|| unsupported("this return type", f.span))?,
    };
    let sret = ret_kind != Kind::Word;
    if f.params.len() + sret as usize > 6 {
        return Err(unsupported("more than 6 parameters", f.span));
    }

    let mut lo = Lowerer {
        res,
        strings,
        module,
        insts: Vec::new(),
        scopes: vec![HashMap::new()],
        vregs: 0,
        floats: Vec::new(),
        labels: 0,
        loops: Vec::new(),
        sret: None,
        ret_words: ret_kind.words(),
    };
    // Hidden sret pointer first, then params — every param is one word
    // (scalars/handles by value, structs/strings by pointer; the caller
    // copied into a private temp at evaluation, so the pointer is safe).
    if sret {
        let v = lo.fresh(false);
        lo.sret = Some(v);
    }
    for (p, ty) in f.params.iter().zip(&sig.params) {
        kind_of(ty, res, FUEL).ok_or_else(|| unsupported("parameters of this type", f.span))?;
        let v = lo.fresh(*ty == Type::Float);
        lo.scopes[0].insert(p.name.clone(), v);
    }
    let nparams = lo.vregs;
    for stmt in &f.body {
        lo.stmt(stmt)?;
    }
    // Fall-through for unit functions; value functions always return
    // (checker-proven) so the extra ret is dead.
    let zero = lo.const_word(0);
    lo.insts.push(Inst::Ret(zero));

    let Lowerer {
        insts,
        vregs,
        floats,
        ..
    } = lo;
    Ok(FunctionIr {
        name: f.name.clone(),
        module,
        nparams,
        vregs,
        floats,
        insts,
    })
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

    fn ty(&self, span: &Span) -> Option<&Type> {
        self.res.expr_types.get(span)
    }

    fn is_float(&self, e: &Expr) -> bool {
        matches!(self.ty(&e.span()), Some(Type::Float))
    }

    /// The checker-recorded kind of an expression (Word for `null`,
    /// whose span the checker never types).
    fn kind(&self, e: &Expr, span: Span) -> Result<Kind, Diagnostic> {
        match self.ty(&e.span()) {
            // `null` types as Type::Null (a word: handle 0); untyped
            // spans don't occur in checked programs.
            None | Some(Type::Unit) | Some(Type::Null) => Ok(Kind::Word),
            Some(t) => {
                kind_of(t, self.res, FUEL).ok_or_else(|| unsupported("values of this type", span))
            }
        }
    }

    fn const_word(&mut self, n: i64) -> V {
        let v = self.fresh(false);
        self.insts.push(Inst::Const(v, n));
        v
    }

    fn lea_sym(&mut self, sym: String) -> V {
        let v = self.fresh(false);
        self.insts.push(Inst::LeaSym { dst: v, sym });
        v
    }

    /// Copies a value into fresh private storage — the oracle's copy
    /// points: bindings, call arguments, equality snapshots.
    fn snapshot(&mut self, src: V, words: usize) -> V {
        let t = self.fresh(false);
        self.insts.push(Inst::Temp { dst: t, words });
        self.insts.push(Inst::CopyW { dst: t, src, words });
        t
    }

    fn block(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        self.scopes.push(HashMap::new());
        let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
        self.scopes.pop();
        result
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Diagnostic> {
        match stmt {
            Stmt::Let {
                name, value, ty, ..
            } => {
                if ty.is_some() {
                    // The checker resolved the annotation; gate on that.
                    let declared = self
                        .res
                        .let_types
                        .get(&stmt.span())
                        .ok_or_else(|| unsupported("bindings of this type", stmt.span()))?;
                    kind_of(declared, self.res, FUEL)
                        .ok_or_else(|| unsupported("bindings of this type", stmt.span()))?;
                }
                let kind = self.kind(value, stmt.span())?;
                let v = self.expr(value)?;
                let slot = if kind == Kind::Word {
                    let s = self.fresh(self.floats[v]);
                    self.insts.push(Inst::Copy(s, v));
                    s
                } else {
                    self.snapshot(v, kind.words())
                };
                self.scopes
                    .last_mut()
                    .expect("a scope is always open")
                    .insert(name.clone(), slot);
            }
            Stmt::Assign { target, value, .. } => match target {
                Expr::Ident(name, span) => {
                    let dst = self
                        .lookup(name)
                        .ok_or_else(|| unsupported("this assignment target", *span))?;
                    let kind = self.kind(value, *span)?;
                    let v = self.expr(value)?;
                    if kind == Kind::Word {
                        self.insts.push(Inst::Copy(dst, v));
                    } else {
                        self.insts.push(Inst::CopyW {
                            dst,
                            src: v,
                            words: kind.words(),
                        });
                    }
                }
                // The oracle evaluates the value before the target.
                Expr::Index { base, index, .. } => {
                    let val = self.expr(value)?;
                    let arr = self.expr(base)?;
                    let idx = self.expr(index)?;
                    self.insts.push(Inst::IndexSet { arr, idx, val });
                }
                Expr::Field { base, span, .. } => {
                    let slot = self
                        .res
                        .field_slots
                        .get(span)
                        .ok_or_else(|| unsupported("this field target", *span))?;
                    let kind = kind_of(&slot.ty, self.res, FUEL)
                        .ok_or_else(|| unsupported("fields of this type", *span))?;
                    let def = &self.res.structs[&slot.base];
                    let off = offset_of(def, slot.index, self.res)
                        .ok_or_else(|| unsupported("this struct layout", *span))?;
                    let val = self.expr(value)?;
                    let b = self.expr(base)?;
                    if kind == Kind::Word {
                        self.insts.push(Inst::StoreAt { base: b, off, val });
                    } else {
                        let p = self.fresh(false);
                        self.insts.push(Inst::LeaAt {
                            dst: p,
                            base: b,
                            off,
                        });
                        self.insts.push(Inst::CopyW {
                            dst: p,
                            src: val,
                            words: kind.words(),
                        });
                    }
                }
                other => return Err(unsupported("this assignment target", other.span())),
            },
            Stmt::Return { value, .. } => match value {
                Some(expr) => {
                    let v = self.expr(expr)?;
                    if let Some(sret) = self.sret {
                        self.insts.push(Inst::CopyW {
                            dst: sret,
                            src: v,
                            words: self.ret_words,
                        });
                        self.insts.push(Inst::Ret(sret));
                    } else {
                        self.insts.push(Inst::Ret(v));
                    }
                }
                None => {
                    let zero = self.const_word(0);
                    self.insts.push(Inst::Ret(zero));
                }
            },
            Stmt::Break { .. } => {
                let (_, brk) = *self.loops.last().expect("checker: loops only");
                self.insts.push(Inst::Jmp(brk));
            }
            Stmt::Continue { .. } => {
                let (cont, _) = *self.loops.last().expect("checker: loops only");
                self.insts.push(Inst::Jmp(cont));
            }
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
                self.loops.push((top, end));
                let result = self.block(body);
                self.loops.pop();
                result?;
                self.insts.push(Inst::Jmp(top));
                self.insts.push(Inst::Label(end));
            }
            Stmt::For {
                index,
                name,
                iterable,
                body,
                ..
            } => {
                // Live iteration, the oracle's contract: length re-read
                // every step, element copied out before the body runs.
                let elem_float = matches!(
                    self.ty(&iterable.span()),
                    Some(Type::Array(inner)) if **inner == Type::Float
                );
                let arr = self.expr(iterable)?;
                let i = self.const_word(0);
                let x = self.fresh(elem_float);
                let top = self.fresh_label();
                let end = self.fresh_label();
                self.insts.push(Inst::Label(top));
                let n = self.fresh(false);
                self.insts.push(Inst::Len(n, arr));
                let cond = self.fresh(false);
                self.insts.push(Inst::Bin {
                    op: BinOp::Lt,
                    float: false,
                    dst: cond,
                    lhs: i,
                    rhs: n,
                });
                self.insts.push(Inst::BrZero(cond, end));
                self.insts.push(Inst::Index {
                    dst: x,
                    arr,
                    idx: i,
                });
                let cont = self.fresh_label();
                let mut bindings = HashMap::new();
                bindings.insert(name.clone(), x);
                if let Some(ix) = index {
                    bindings.insert(ix.clone(), i);
                }
                self.scopes.push(bindings);
                self.loops.push((cont, end));
                let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
                self.loops.pop();
                self.scopes.pop();
                result?;
                self.insts.push(Inst::Label(cont));
                self.insts.push(Inst::BinImm {
                    op: BinOp::Add,
                    dst: i,
                    lhs: i,
                    imm: 1,
                });
                self.insts.push(Inst::Jmp(top));
                self.insts.push(Inst::Label(end));
            }
            Stmt::Expr(expr) => {
                self.expr(expr)?;
            }
        }
        Ok(())
    }

    fn expr(&mut self, expr: &Expr) -> Result<V, Diagnostic> {
        match expr {
            Expr::Int(n, _) => Ok(self.const_word(*n)),
            Expr::Bool(b, _) => Ok(self.const_word(*b as i64)),
            Expr::Float(f, _) => {
                let v = self.fresh(true);
                self.insts.push(Inst::Const(v, f.to_bits() as i64));
                Ok(v)
            }
            // `null` is handle 0 — sound because value-typed optionals
            // never compile (annotation, param, field, and array-literal
            // gates), so 0 is never a legitimate optional payload.
            Expr::Null(_) => Ok(self.const_word(0)),
            // A literal's descriptor and bytes are both static — strings
            // are immutable, so every use shares one rodata object.
            Expr::Str(text, _) => {
                let sym = self.strings.intern(text);
                Ok(self.lea_sym(sym))
            }
            Expr::Ident(name, span) => self
                .lookup(name)
                .ok_or_else(|| unsupported("this name", *span)),
            Expr::Unary { op, rhs, .. } => {
                let float = self.is_float(rhs);
                let r = self.expr(rhs)?;
                let v = self.fresh(float);
                self.insts.push(match op {
                    UnOp::Neg if float => Inst::NegF(v, r),
                    UnOp::Neg => Inst::Neg(v, r),
                    UnOp::Not => Inst::Not(v, r),
                });
                Ok(v)
            }
            Expr::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
                ..
            } => {
                // Short-circuit: the left side IS the result when it
                // decides; the right side stays lazy, like the oracle.
                let v = self.fresh(false);
                let l = self.expr(lhs)?;
                self.insts.push(Inst::Copy(v, l));
                let end = self.fresh_label();
                if matches!(op, BinOp::And) {
                    self.insts.push(Inst::BrZero(v, end));
                } else {
                    let inv = self.fresh(false);
                    self.insts.push(Inst::Not(inv, v));
                    self.insts.push(Inst::BrZero(inv, end));
                }
                let r = self.expr(rhs)?;
                self.insts.push(Inst::Copy(v, r));
                self.insts.push(Inst::Label(end));
                Ok(v)
            }
            Expr::Binary {
                op: BinOp::Coalesce,
                lhs,
                rhs,
                ..
            } => {
                // Handles only (value optionals never compile): keep the
                // left side unless it is null (0). The null test composes
                // from BinImm because handles aren't 0/1 booleans.
                let v = self.fresh(false);
                let l = self.expr(lhs)?;
                self.insts.push(Inst::Copy(v, l));
                let end = self.fresh_label();
                let isnull = self.fresh(false);
                self.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: isnull,
                    lhs: v,
                    imm: 0,
                });
                self.insts.push(Inst::BrZero(isnull, end));
                let r = self.expr(rhs)?;
                self.insts.push(Inst::Copy(v, r));
                self.insts.push(Inst::Label(end));
                Ok(v)
            }
            Expr::Binary { op, lhs, rhs, span } => self.binary(*op, lhs, rhs, *span),
            Expr::Call { callee, args, span } => self.call(callee, args, *span),
            Expr::ArrayLit { elements, span } => {
                // A null element could make the literal an `int?[]` — a
                // value-optional array the word model can't represent.
                if let Some(null) = elements.iter().find(|e| matches!(e, Expr::Null(_))) {
                    return Err(unsupported(
                        "array literals with null elements",
                        null.span(),
                    ));
                }
                for e in elements {
                    if self.kind(e, *span)? != Kind::Word {
                        return Err(unsupported("arrays of multi-word values", e.span()));
                    }
                }
                // Header {len, cap, data*} plus buffer, per ADR 0014.
                let c24 = self.const_word(24);
                let hdr = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: hdr,
                    sym: "malloc@PLT",
                    args: vec![c24],
                    varargs: false,
                });
                let size = self.const_word(8 * elements.len().max(1) as i64);
                let buf = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: buf,
                    sym: "malloc@PLT",
                    args: vec![size],
                    varargs: false,
                });
                self.insts.push(Inst::StoreHdr {
                    hdr,
                    buf,
                    len: elements.len(),
                });
                for (slot, element) in elements.iter().enumerate() {
                    let val = self.expr(element)?;
                    self.insts.push(Inst::BufSet { buf, slot, val });
                }
                Ok(hdr)
            }
            Expr::Index { base, index, span } => {
                let arr = self.expr(base)?;
                let idx = self.expr(index)?;
                let dst = self.fresh(matches!(self.ty(span), Some(Type::Float)));
                self.insts.push(Inst::Index { dst, arr, idx });
                Ok(dst)
            }
            Expr::Field {
                base,
                optional,
                span,
                ..
            } => {
                let slot = self
                    .res
                    .field_slots
                    .get(span)
                    .ok_or_else(|| unsupported("this field access", *span))?;
                let kind = kind_of(&slot.ty, self.res, FUEL)
                    .ok_or_else(|| unsupported("fields of this type", *span))?;
                let def = &self.res.structs[&slot.base];
                let off = offset_of(def, slot.index, self.res)
                    .ok_or_else(|| unsupported("this struct layout", *span))?;
                // `p?.x` with a value-typed x yields `int?` — a value
                // optional the word model can't represent. Handle-typed
                // fields are fine, already-optional ones stay flat.
                let unwrapped = match &slot.ty {
                    Type::Optional(inner) => inner.as_ref(),
                    other => other,
                };
                if *optional && !ref_shaped(unwrapped, self.res) {
                    return Err(unsupported("'?.' on a field of value type", *span));
                }
                let float = matches!(slot.ty, Type::Float);
                let b = self.expr(base)?;
                if *optional {
                    // Null short-circuits to null (0 stays the result).
                    let r = self.const_word(0);
                    let end = self.fresh_label();
                    self.insts.push(Inst::BrZero(b, end));
                    self.insts.push(Inst::LoadAt {
                        dst: r,
                        base: b,
                        off,
                    });
                    self.insts.push(Inst::Label(end));
                    Ok(r)
                } else if kind == Kind::Word {
                    // No null check: the checker's narrowing is sound, so
                    // a plain `.` base is proven non-null (ADR 0007).
                    let r = self.fresh(float);
                    self.insts.push(Inst::LoadAt {
                        dst: r,
                        base: b,
                        off,
                    });
                    Ok(r)
                } else {
                    // A struct/str-typed field's value is its storage
                    // inside the base — an interior pointer; consumers
                    // copy.
                    let r = self.fresh(false);
                    self.insts.push(Inst::LeaAt {
                        dst: r,
                        base: b,
                        off,
                    });
                    Ok(r)
                }
            }
            Expr::StructLit { fields, span, .. } => {
                let Some(Type::Struct(dm, dn)) = self.ty(span) else {
                    return Err(unsupported("this struct literal", *span));
                };
                let key = (*dm, dn.clone());
                let res = self.res;
                let def = &res.structs[&key];
                let kinds: Vec<Kind> = def
                    .fields
                    .iter()
                    .map(|(_, t)| kind_of(t, self.res, FUEL))
                    .collect::<Option<_>>()
                    .ok_or_else(|| unsupported("structs with fields of this type", *span))?;
                let offsets: Vec<i64> = kinds
                    .iter()
                    .scan(0i64, |acc, k| {
                        let off = *acc;
                        *acc += 8 * k.words() as i64;
                        Some(off)
                    })
                    .collect();
                let total: usize = kinds.iter().map(|k| k.words()).sum();
                let base = if def.by_ref {
                    // One heap object; the checker proved the literal
                    // complete, so every slot is written.
                    let size = self.const_word((8 * total).max(8) as i64);
                    let hdr = self.fresh(false);
                    self.insts.push(Inst::CallRt {
                        dst: hdr,
                        sym: "malloc@PLT",
                        args: vec![size],
                        varargs: false,
                    });
                    hdr
                } else {
                    // A value literal builds in a frame temp; its static
                    // address means no handle juggling at all.
                    let t = self.fresh(false);
                    self.insts.push(Inst::Temp {
                        dst: t,
                        words: total.max(1),
                    });
                    t
                };
                for (fname, value) in fields {
                    let i = def
                        .fields
                        .iter()
                        .position(|(dn, _)| dn == fname)
                        .expect("checker verified the field exists");
                    let val = self.expr(value)?;
                    if kinds[i] == Kind::Word {
                        self.insts.push(Inst::StoreAt {
                            base,
                            off: offsets[i],
                            val,
                        });
                    } else {
                        let p = self.fresh(false);
                        self.insts.push(Inst::LeaAt {
                            dst: p,
                            base,
                            off: offsets[i],
                        });
                        self.insts.push(Inst::CopyW {
                            dst: p,
                            src: val,
                            words: kinds[i].words(),
                        });
                    }
                }
                Ok(base)
            }
        }
    }
    /// One binary operator, dispatched by the left operand's kind:
    /// string `+` concatenates, aggregate `==`/`!=` compares content or
    /// structure, everything else is scalar arithmetic.
    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Result<V, Diagnostic> {
        let kind = self.kind(lhs, span)?;
        if kind == Kind::Str && matches!(op, BinOp::Add) {
            return self.concat(lhs, rhs);
        }
        if matches!(op, BinOp::Eq | BinOp::Ne) && kind != Kind::Word {
            return self.aggregate_eq(op, kind, lhs, rhs, span);
        }
        self.scalar_binary(op, lhs, rhs)
    }

    /// `a + b` on strings — the one explicitly allocating string
    /// operation (ADR 0013): new buffer, both byte runs copied, fresh
    /// descriptor in a statement temp.
    fn concat(&mut self, lhs: &Expr, rhs: &Expr) -> Result<V, Diagnostic> {
        let l = self.expr(lhs)?;
        let r = self.expr(rhs)?;
        let la = self.load_at(l, 8);
        let lb = self.load_at(r, 8);
        let total = self.fresh(false);
        self.insts.push(Inst::Bin {
            op: BinOp::Add,
            float: false,
            dst: total,
            lhs: la,
            rhs: lb,
        });
        let buf = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: buf,
            sym: "malloc@PLT",
            args: vec![total],
            varargs: false,
        });
        let pa = self.load_at(l, 0);
        let d1 = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: d1,
            sym: "memcpy@PLT",
            args: vec![buf, pa, la],
            varargs: false,
        });
        let mid = self.fresh(false);
        self.insts.push(Inst::Bin {
            op: BinOp::Add,
            float: false,
            dst: mid,
            lhs: buf,
            rhs: la,
        });
        let pb = self.load_at(r, 0);
        let d2 = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: d2,
            sym: "memcpy@PLT",
            args: vec![mid, pb, lb],
            varargs: false,
        });
        let out = self.fresh(false);
        self.insts.push(Inst::Temp { dst: out, words: 2 });
        self.insts.push(Inst::StoreAt {
            base: out,
            off: 0,
            val: buf,
        });
        self.insts.push(Inst::StoreAt {
            base: out,
            off: 8,
            val: total,
        });
        Ok(out)
    }

    /// `==`/`!=` on strings (content: length then bytes) and value
    /// structs (structural: one memcmp over the padding-free words).
    /// The right side may mutate the left side's storage through an
    /// alias; the oracle compares the value from before — so the left
    /// operand is snapshotted first.
    fn aggregate_eq(
        &mut self,
        op: BinOp,
        kind: Kind,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Result<V, Diagnostic> {
        // The right side may mutate the left side's storage through
        // an alias; the oracle compares the value from before — so
        // snapshot the left operand first.
        let eq = match kind {
            // Content equality (ADR 0013): length first, then bytes.
            Kind::Str => {
                let l = self.expr(lhs)?;
                let snap = self.snapshot(l, 2);
                let r = self.expr(rhs)?;
                let la = self.load_at(snap, 8);
                let lb = self.load_at(r, 8);
                let result = self.fresh(false);
                self.insts.push(Inst::Bin {
                    op: BinOp::Eq,
                    float: false,
                    dst: result,
                    lhs: la,
                    rhs: lb,
                });
                let end = self.fresh_label();
                self.insts.push(Inst::BrZero(result, end));
                let pa = self.load_at(snap, 0);
                let pb = self.load_at(r, 0);
                let cmp = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: cmp,
                    sym: "memcmp@PLT",
                    args: vec![pa, pb, la],
                    varargs: false,
                });
                self.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: result,
                    lhs: cmp,
                    imm: 0,
                });
                self.insts.push(Inst::Label(end));
                result
            }
            // str fields compare by content and float fields by
            // IEEE — one memcmp decides neither.
            Kind::Struct {
                no_memcmp: true, ..
            } => {
                return Err(unsupported(
                    "'==' on structs containing strings or floats",
                    span,
                ));
            }
            // Value-struct equality is structural: the layout is
            // padding-free 8-byte words, so memcmp decides it.
            Kind::Struct { words, .. } => {
                let l = self.expr(lhs)?;
                let snap = self.snapshot(l, words);
                let r = self.expr(rhs)?;
                let n = self.const_word(8 * words as i64);
                let cmp = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: cmp,
                    sym: "memcmp@PLT",
                    args: vec![snap, r, n],
                    varargs: false,
                });
                let result = self.fresh(false);
                self.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: result,
                    lhs: cmp,
                    imm: 0,
                });
                result
            }
            Kind::Word => unreachable!(),
        };
        if matches!(op, BinOp::Ne) {
            let inv = self.fresh(false);
            self.insts.push(Inst::Not(inv, eq));
            return Ok(inv);
        }
        Ok(eq)
    }

    /// Scalar arithmetic and comparisons — IEEE via SSE for floats,
    /// wrapping two's complement for ints, with constant right operands
    /// strength-reduced (immediates, shift sequences, magic multiplies).
    fn scalar_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<V, Diagnostic> {
        let float = self.is_float(lhs);
        let arith = !matches!(
            op,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        );
        // Constant right operands strength-reduce: power-of-two div/rem
        // to shift sequences, other divisors to magic multiplies,
        // everything else to an immediate form (i32 range).
        if !float {
            if let Expr::Int(n, _) = rhs {
                let imm_ok = i32::try_from(*n).is_ok();
                // k > 31 would put the 2^k - 1 bias outside leaq's 32-bit
                // displacement; those divisors take the magic path.
                let pow2 = *n >= 2 && (*n & (*n - 1)) == 0 && n.trailing_zeros() <= 31;
                match op {
                    BinOp::Div | BinOp::Rem if pow2 => {
                        let l = self.expr(lhs)?;
                        let v = self.fresh(false);
                        let k = n.trailing_zeros();
                        self.insts.push(if matches!(op, BinOp::Div) {
                            Inst::DivPow2 { dst: v, src: l, k }
                        } else {
                            Inst::RemPow2 { dst: v, src: l, k }
                        });
                        return Ok(v);
                    }
                    BinOp::Div | BinOp::Rem if *n >= 2 => {
                        let l = self.expr(lhs)?;
                        let v = self.fresh(false);
                        self.insts.push(if matches!(op, BinOp::Div) {
                            Inst::DivMagic {
                                dst: v,
                                src: l,
                                d: *n,
                            }
                        } else {
                            Inst::RemMagic {
                                dst: v,
                                src: l,
                                d: *n,
                            }
                        });
                        return Ok(v);
                    }
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
                        if imm_ok =>
                    {
                        let l = self.expr(lhs)?;
                        let v = self.fresh(false);
                        self.insts.push(Inst::BinImm {
                            op,
                            dst: v,
                            lhs: l,
                            imm: *n,
                        });
                        return Ok(v);
                    }
                    _ => {}
                }
            }
            if let Expr::Int(n, _) = lhs
                && matches!(op, BinOp::Add | BinOp::Mul)
                && i32::try_from(*n).is_ok()
            {
                let r = self.expr(rhs)?;
                let v = self.fresh(false);
                self.insts.push(Inst::BinImm {
                    op,
                    dst: v,
                    lhs: r,
                    imm: *n,
                });
                return Ok(v);
            }
        }
        let l = self.expr(lhs)?;
        let r = self.expr(rhs)?;
        let v = self.fresh(float && arith);
        self.insts.push(Inst::Bin {
            op,
            float,
            dst: v,
            lhs: l,
            rhs: r,
        });
        Ok(v)
    }

    fn load_at(&mut self, base: V, off: i64) -> V {
        let dst = self.fresh(false);
        self.insts.push(Inst::LoadAt { dst, base, off });
        dst
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Result<V, Diagnostic> {
        let Expr::Ident(name, _) = callee else {
            return Err(unsupported("this callee", span));
        };
        // Resolution order: builtins only when no user definition.
        let Some(key) = self.res.functions[self.module].get(name).cloned() else {
            return self.builtin(name, args, span);
        };
        let res = self.res;
        let sig = &res.sigs[&key];
        let ret_kind = match &sig.ret {
            Type::Unit => Kind::Word,
            t => kind_of(t, self.res, FUEL)
                .ok_or_else(|| unsupported("calls returning this type", span))?,
        };
        let sret = ret_kind != Kind::Word;
        if args.len() + sret as usize > 6 {
            return Err(unsupported("calls with more than 6 arguments", span));
        }
        // Multi-word arguments copy into private temps AT EVALUATION
        // TIME: the oracle copies as it evaluates, so a later argument
        // mutating the storage through an alias must not be visible.
        let mut arg_vregs = Vec::new();
        for arg in args {
            let kind = self.kind(arg, span)?;
            let v = self.expr(arg)?;
            arg_vregs.push(if kind == Kind::Word {
                v
            } else {
                self.snapshot(v, kind.words())
            });
        }
        let sret_temp = sret.then(|| {
            let t = self.fresh(false);
            self.insts.push(Inst::Temp {
                dst: t,
                words: ret_kind.words(),
            });
            t
        });
        let dst = self.fresh(sig.ret == Type::Float);
        self.insts.push(Inst::Call {
            dst,
            label: label_of(key.0, &key.1),
            args: arg_vregs,
            sret: sret_temp,
        });
        Ok(dst)
    }

    fn builtin(&mut self, name: &str, args: &[Expr], span: Span) -> Result<V, Diagnostic> {
        match (name, args) {
            ("len", [array]) => {
                let arr = self.expr(array)?;
                let dst = self.fresh(false);
                self.insts.push(Inst::Len(dst, arr));
                Ok(dst)
            }
            ("push", [array, value]) => {
                if self.kind(value, span)? != Kind::Word {
                    return Err(unsupported("arrays of multi-word values", value.span()));
                }
                let arr = self.expr(array)?;
                let val = self.expr(value)?;
                let dst = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst,
                    sym: "ys_push",
                    args: vec![arr, val],
                    varargs: false,
                });
                Ok(dst)
            }
            ("print", [value]) => {
                let ty = self
                    .ty(&value.span())
                    .cloned()
                    .ok_or_else(|| unsupported("printing this value", span))?;
                match ty {
                    Type::Int => {
                        let v = self.expr(value)?;
                        let fmt = self.lea_sym(".Lfmt_int".into());
                        let dst = self.fresh(false);
                        self.insts.push(Inst::CallRt {
                            dst,
                            sym: "printf@PLT",
                            args: vec![fmt, v],
                            varargs: true,
                        });
                        Ok(dst)
                    }
                    Type::Bool => {
                        let v = self.expr(value)?;
                        let r = self.lea_sym(".Ltrue_s".into());
                        // Swap in "false" when v is zero.
                        let end = self.fresh_label();
                        let isfalse = self.fresh(false);
                        self.insts.push(Inst::BinImm {
                            op: BinOp::Eq,
                            dst: isfalse,
                            lhs: v,
                            imm: 0,
                        });
                        self.insts.push(Inst::BrZero(isfalse, end));
                        let f = self.fresh(false);
                        self.insts.push(Inst::LeaSym {
                            dst: f,
                            sym: ".Lfalse_s".into(),
                        });
                        self.insts.push(Inst::Copy(r, f));
                        self.insts.push(Inst::Label(end));
                        let fmt = self.lea_sym(".Lfmt_cstr".into());
                        let dst = self.fresh(false);
                        self.insts.push(Inst::CallRt {
                            dst,
                            sym: "printf@PLT",
                            args: vec![fmt, r],
                            varargs: true,
                        });
                        Ok(dst)
                    }
                    // Length-carried, so %.*s with (len, ptr).
                    Type::Str => {
                        let v = self.expr(value)?;
                        let len = self.load_at(v, 8);
                        let ptr = self.load_at(v, 0);
                        let fmt = self.lea_sym(".Lfmt_str".into());
                        let dst = self.fresh(false);
                        self.insts.push(Inst::CallRt {
                            dst,
                            sym: "printf@PLT",
                            args: vec![fmt, len, ptr],
                            varargs: true,
                        });
                        Ok(dst)
                    }
                    // Formatting parity with Rust's f64 Display is its
                    // own project; aggregates need the debug renderer.
                    Type::Float => Err(unsupported("printing floats", span)),
                    _ => Err(unsupported("printing values of this type", span)),
                }
            }
            _ => Err(unsupported(&format!("builtin '{name}'"), span)),
        }
    }
}
