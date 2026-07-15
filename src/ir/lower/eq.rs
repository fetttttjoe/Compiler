//! The equality matrix (ADR 0021/0026). Expression-level concerns —
//! evaluation order, the left-operand snapshot, null literals, the
//! mixed `T? == T` shapes — live in `aggregate_eq`/`optional_eq`;
//! both delegate every value comparison to `value_eq`, one structural
//! comparator dispatching on layout kind: word compare, string
//! content, one memcmp where canonical layout allows it, per-field
//! and per-payload walks where it doesn't.

use super::Lowerer;
use crate::ast::{BinOp, Expr};
use crate::codegen::RT_MEMCMP;
use crate::diagnostic::Diagnostic;
use crate::ir::layout::{FUEL, Kind, kind_of, offset_of};
use crate::ir::{Inst, V, unsupported};
use crate::span::Span;
use crate::types::Type;

impl Lowerer<'_> {
    /// The payload type when the expression's recorded type is a value
    /// optional.
    pub(super) fn value_opt_inner(&self, e: &Expr) -> Option<Type> {
        self.ty(&e.span())
            .cloned()
            .and_then(|t| self.opt_inner_of(&t))
    }

    /// `==`/`!=` on strings and value structs. The right side may
    /// mutate the left side's storage through an alias; the oracle
    /// compares the value from before — so the left operand is
    /// snapshotted first. The comparison itself is `value_eq`.
    pub(super) fn aggregate_eq(
        &mut self,
        op: BinOp,
        ty: &Type,
        kind: Kind,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Result<V, Diagnostic> {
        let l = self.expr(lhs)?;
        let snap = self.snapshot(l, kind.words());
        let r = self.expr(rhs)?;
        let eq = self.value_eq(ty, snap, 0, r, 0, span)?;
        Ok(self.negate_if_ne(op, eq))
    }

    fn negate_if_ne(&mut self, op: BinOp, eq: V) -> V {
        if matches!(op, BinOp::Ne) {
            let inv = self.fresh(false);
            self.insts.push(Inst::Not(inv, eq));
            return inv;
        }
        eq
    }

    /// `==`/`!=` where at least one side is a tagged value optional:
    /// presence first, then the payload by its class. The left operand
    /// snapshots before the right evaluates, like every aggregate.
    pub(super) fn optional_eq(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        l_opt: Option<Type>,
        r_opt: Option<Type>,
        span: Span,
    ) -> Result<V, Diagnostic> {
        let inner = l_opt
            .clone()
            .or_else(|| r_opt.clone())
            .expect("routed on an optional side");
        let ik = kind_of(&inner, self.res, FUEL)
            .ok_or_else(|| unsupported("values of this type", span))?;
        let l_null = matches!(self.ty(&lhs.span()), None | Some(Type::Null));
        let r_null = matches!(self.ty(&rhs.span()), None | Some(Type::Null));
        let eq = if l_null || r_null {
            // `x == null`: a pure tag test (ADR 0021 decision 5) —
            // legal for every payload class. The null literal itself
            // lowers to nothing.
            let side = if r_null { lhs } else { rhs };
            let v = self.expr(side)?;
            let tag = self.load_at(v, 0);
            let isnull = self.fresh(false);
            self.insts.push(Inst::BinImm {
                op: BinOp::Eq,
                dst: isnull,
                lhs: tag,
                imm: 0,
            });
            isnull
        } else {
            match (l_opt.is_some(), r_opt.is_some()) {
                // T? == T?: one whole-value comparison — value_eq's
                // optional leg is tags-then-payload (or one memcmp,
                // canonical nulls permitting).
                (true, true) => {
                    let lt = Type::Optional(Box::new(inner.clone()));
                    let l = self.expr(lhs)?;
                    let snap = self.snapshot(l, 1 + ik.words());
                    let r = self.expr(rhs)?;
                    self.value_eq(&lt, snap, 0, r, 0, span)?
                }
                // T? == T: present, and the payload equals the bare
                // value. The optional side snapshots when it lowers
                // first; the bare side when it is multi-word.
                (true, false) => {
                    let l = self.expr(lhs)?;
                    let snap = self.snapshot(l, 1 + ik.words());
                    let r = self.expr(rhs)?;
                    self.opt_vs_bare_eq(&inner, ik, snap, r, span)?
                }
                (false, true) => {
                    let l = self.expr(lhs)?;
                    let l = if ik == Kind::Word {
                        l
                    } else {
                        self.snapshot(l, ik.words())
                    };
                    let r = self.expr(rhs)?;
                    self.opt_vs_bare_eq(&inner, ik, r, l, span)?
                }
                (false, false) => unreachable!("routed on an optional side"),
            }
        };
        Ok(self.negate_if_ne(op, eq))
    }

    /// `T? == T` (either operand order — the legs are symmetric):
    /// present, and the payload equals the bare value.
    fn opt_vs_bare_eq(
        &mut self,
        inner: &Type,
        ik: Kind,
        opt: V,
        bare: V,
        span: Span,
    ) -> Result<V, Diagnostic> {
        let tag = self.load_at(opt, 0);
        let v = self.fresh(false);
        self.insts.push(Inst::BinImm {
            op: BinOp::Eq,
            dst: v,
            lhs: tag,
            imm: 1,
        });
        let end = self.fresh_label();
        self.insts.push(Inst::BrZero(v, end));
        let pe = if ik == Kind::Word {
            // The bare word rides in its vreg; the payload sits at +8.
            let pv = self.load_at(opt, 8);
            let e = self.fresh(false);
            self.insts.push(Inst::Bin {
                op: BinOp::Eq,
                float: *inner == Type::Float,
                dst: e,
                lhs: pv,
                rhs: bare,
            });
            e
        } else {
            // Multi-word bare values travel as pointers — address
            // semantics line up with the payload at opt+8.
            self.value_eq(inner, opt, 8, bare, 0, span)?
        };
        self.insts.push(Inst::Copy(v, pe));
        self.insts.push(Inst::Label(end));
        Ok(v)
    }

    /// Structural equality of two same-typed values addressed as
    /// pointer + byte offset: word kinds load both sides, multi-word
    /// kinds compare through interior pointers. One memcmp where the
    /// layout allows it (padding-free words, canonical nulls);
    /// content, IEEE, and per-field legs where it doesn't (ADR 0026).
    /// Value structs cannot be recursive, so the walk is finite.
    fn value_eq(
        &mut self,
        t: &Type,
        a: V,
        aoff: i64,
        b: V,
        boff: i64,
        span: Span,
    ) -> Result<V, Diagnostic> {
        let kind =
            kind_of(t, self.res, FUEL).ok_or_else(|| unsupported("values of this type", span))?;
        match kind {
            // Scalars by value, refstructs/arrays by handle identity.
            Kind::Word => {
                let av = self.load_at(a, aoff);
                let bv = self.load_at(b, boff);
                let v = self.fresh(false);
                self.insts.push(Inst::Bin {
                    op: BinOp::Eq,
                    float: *t == Type::Float,
                    dst: v,
                    lhs: av,
                    rhs: bv,
                });
                Ok(v)
            }
            // Content equality (ADR 0013): length first, then bytes.
            Kind::Str => {
                let pa = self.ptr_at(a, aoff);
                let pb = self.ptr_at(b, boff);
                Ok(self.str_content_eq(pa, pb))
            }
            Kind::Struct {
                no_memcmp: false,
                words,
            }
            | Kind::Opt {
                no_memcmp: false,
                words,
            } => {
                let pa = self.ptr_at(a, aoff);
                let pb = self.ptr_at(b, boff);
                let n = self.const_word(8 * words as i64);
                let cmp = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: cmp,
                    sym: RT_MEMCMP,
                    args: vec![pa, pb, n],
                    varargs: false,
                });
                let v = self.fresh(false);
                self.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: v,
                    lhs: cmp,
                    imm: 0,
                });
                Ok(v)
            }
            // String content and IEEE floats rule memcmp out: walk
            // the fields, short-circuiting on the first mismatch —
            // pure loads and compares, so the early exit is
            // unobservable.
            Kind::Struct {
                no_memcmp: true, ..
            } => {
                let Type::Struct(m, n) = t else {
                    unreachable!("struct kind from a struct type")
                };
                let res = self.res;
                let def = &res.structs[&(*m, n.clone())];
                let legs: Vec<(i64, Type)> = def
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, (_, ft))| Some((offset_of(def, i, res)?, ft.clone())))
                    .collect::<Option<_>>()
                    .ok_or_else(|| unsupported("values of this type", span))?;
                let v = self.const_word(1);
                let end = self.fresh_label();
                for (off, ft) in legs {
                    let fe = self.value_eq(&ft, a, aoff + off, b, boff + off, span)?;
                    self.insts.push(Inst::Copy(v, fe));
                    self.insts.push(Inst::BrZero(v, end));
                }
                self.insts.push(Inst::Label(end));
                Ok(v)
            }
            // Tags equal, and both null or the payloads equal.
            Kind::Opt {
                no_memcmp: true, ..
            } => {
                let Type::Optional(inner) = t else {
                    unreachable!("opt kind from an optional type")
                };
                let inner = (**inner).clone();
                let ta = self.load_at(a, aoff);
                let tb = self.load_at(b, boff);
                let v = self.fresh(false);
                self.insts.push(Inst::Bin {
                    op: BinOp::Eq,
                    float: false,
                    dst: v,
                    lhs: ta,
                    rhs: tb,
                });
                let end = self.fresh_label();
                self.insts.push(Inst::BrZero(v, end));
                self.insts.push(Inst::BrZero(ta, end));
                let pe = self.value_eq(&inner, a, aoff + 8, b, boff + 8, span)?;
                self.insts.push(Inst::Copy(v, pe));
                self.insts.push(Inst::Label(end));
                Ok(v)
            }
        }
    }

    /// `base + off` as a pointer; offset 0 is the pointer itself.
    fn ptr_at(&mut self, base: V, off: i64) -> V {
        if off == 0 {
            base
        } else {
            self.lea_at(base, off)
        }
    }

    /// Content equality of two string descriptors: length, then bytes.
    fn str_content_eq(&mut self, a: V, b: V) -> V {
        let la = self.load_at(a, 8);
        let lb = self.load_at(b, 8);
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
        let pa = self.load_at(a, 0);
        let pb = self.load_at(b, 0);
        let cmp = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: cmp,
            sym: RT_MEMCMP,
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
}
