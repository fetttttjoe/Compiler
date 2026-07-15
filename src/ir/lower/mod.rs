//! AST → IR. One pass over the checked function body; every construct
//! becomes a handful of [`Inst`]s in the oracle's evaluation order.
//! Anything the backend can't represent yet returns a clean
//! "not yet compilable" diagnostic — there is no fallback path.

use super::layout::{FUEL, Kind, kind_of, offset_of, ref_shaped};
use super::show::{DEPTH_BUDGET, Printers};
use super::{FunctionIr, Inst, Lbl, V, unsupported};
use crate::ast::{BinOp, Conv, Expr, Function, Stmt, UnOp};
use crate::check::Resolutions;
use crate::codegen::{
    FALSE_S, FMT_CSTR, FMT_INT, FMT_STR, NULL_S, RT_FMT_F64, RT_MALLOC, RT_MEMCPY, RT_PRINTF,
    RT_PUSH, RT_PUSH_N, RT_SB_INT, SB_HDR, Strings, TRUE_S, label_of,
};
use crate::diagnostic::Diagnostic;
use crate::source::SourceMap;
use crate::span::Span;
use crate::syntax;
use crate::types::Type;
use std::collections::HashMap;

mod eq;

pub(super) struct Lowerer<'a> {
    pub(super) res: &'a Resolutions,
    pub(super) strings: &'a mut Strings,
    pub(super) printers: &'a mut Printers,
    pub(super) map: &'a SourceMap,
    pub(super) module: usize,
    pub(super) insts: Vec<Inst>,
    pub(super) scopes: Vec<HashMap<String, Binding>>,
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
    pub(super) ret_ty: Type,
}

/// A lowered binding: its storage vreg plus the payload type when the
/// storage is a tagged value optional — narrowed reads unwrap through
/// it (ADR 0021 decision 3).
#[derive(Clone)]
pub(super) struct Binding {
    v: V,
    opt_inner: Option<Type>,
}

/// Lowers one checked function into owned virtual-register IR.
pub(super) fn lower(
    f: &Function,
    module: usize,
    res: &Resolutions,
    strings: &mut Strings,
    printers: &mut Printers,
    map: &SourceMap,
) -> Result<FunctionIr, Diagnostic> {
    let sig = &res.sigs[&(module, f.name.clone())];
    let ret_kind = match &sig.ret {
        Type::Unit => Kind::Word,
        t => kind_of(t, res, FUEL).ok_or_else(|| unsupported("this return type", f.span))?,
    };
    let sret = ret_kind != Kind::Word;

    let mut lo = Lowerer {
        res,
        strings,
        printers,
        map,
        module,
        insts: Vec::new(),
        scopes: vec![HashMap::new()],
        vregs: 0,
        floats: Vec::new(),
        labels: 0,
        loops: Vec::new(),
        sret: None,
        ret_words: ret_kind.words(),
        ret_ty: sig.ret.clone(),
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
        let opt_inner = lo.opt_inner_of(ty);
        lo.scopes[0].insert(p.name.clone(), Binding { v, opt_inner });
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

    fn lookup(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    /// The element type behind an array-typed expression's recorded
    /// type. Declared positions re-record literals as the slot's type
    /// (check_literal_against) — outer optionals included (`T[]?`),
    /// so unwrap first; strides then always match consumers.
    fn elem_ty(&self, e: &Expr) -> Result<Type, Diagnostic> {
        let mut t = self
            .ty(&e.span())
            .cloned()
            .ok_or_else(|| unsupported("this array", e.span()))?;
        while let Type::Optional(inner) = t {
            t = *inner;
        }
        match t {
            Type::Array(inner) => Ok(*inner),
            _ => Err(unsupported("this array", e.span())),
        }
    }

    /// The payload type when `ty` lowers to Opt-shaped storage.
    fn opt_inner_of(&self, ty: &Type) -> Option<Type> {
        match ty {
            Type::Optional(inner) if !ref_shaped(inner, self.res) => Some((**inner).clone()),
            _ => None,
        }
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

    /// The interned `file:line:col` label for a trap site (ADR 0022).
    fn loc_of(&mut self, span: Span) -> String {
        let r = self.map.resolve(span.start);
        self.strings
            .intern_cstr(&format!("{}:{}:{}", r.file, r.line, r.col))
    }

    /// Copies a value into fresh private storage — the oracle's copy
    /// points: bindings, call arguments, equality snapshots.
    fn snapshot(&mut self, src: V, words: usize) -> V {
        let t = self.fresh(false);
        self.insts.push(Inst::Temp { dst: t, words });
        self.insts.push(Inst::CopyW { dst: t, src, words });
        t
    }

    /// The canonical null of a value optional: every word zeroed — tag
    /// and payload alike — so struct equality can memcmp (ADR 0021).
    fn null_optional(&mut self, words: usize) -> V {
        let t = self.fresh(false);
        self.insts.push(Inst::Temp { dst: t, words });
        let zero = self.const_word(0);
        for i in 0..words {
            self.insts.push(Inst::StoreAt {
                base: t,
                off: 8 * i as i64,
                val: zero,
            });
        }
        t
    }

    /// Wraps a present payload into a fresh `{1, payload}` temp.
    fn wrap_present(&mut self, payload: V, payload_kind: Kind, words: usize) -> V {
        let t = self.fresh(false);
        self.insts.push(Inst::Temp { dst: t, words });
        let one = self.const_word(1);
        self.insts.push(Inst::StoreAt {
            base: t,
            off: 0,
            val: one,
        });
        if payload_kind == Kind::Word {
            self.insts.push(Inst::StoreAt {
                base: t,
                off: 8,
                val: payload,
            });
        } else {
            let p = self.fresh(false);
            self.insts.push(Inst::LeaAt {
                dst: p,
                base: t,
                off: 8,
            });
            self.insts.push(Inst::CopyW {
                dst: p,
                src: payload,
                words: payload_kind.words(),
            });
        }
        t
    }

    /// Lowers `e` as a value for a slot of type `target`, wrapping at
    /// the fits() points (ADR 0021 decision 4): `null` and payload-typed
    /// values become tagged optionals; everything else is unchanged.
    fn expr_into(&mut self, e: &Expr, target: &Type) -> Result<V, Diagnostic> {
        let Type::Optional(inner) = target else {
            return self.expr(e);
        };
        if ref_shaped(inner, self.res) {
            return self.expr(e);
        }
        let total = kind_of(target, self.res, FUEL)
            .ok_or_else(|| unsupported("values of this type", e.span()))?
            .words();
        match self.ty(&e.span()) {
            // The null literal itself — nothing to evaluate.
            None | Some(Type::Null) => Ok(self.null_optional(total)),
            // Already optional-shaped: pass the pointer through.
            Some(Type::Optional(_)) => self.expr(e),
            Some(_) => {
                let k = kind_of(inner, self.res, FUEL)
                    .ok_or_else(|| unsupported("values of this type", e.span()))?;
                let v = self.expr(e)?;
                Ok(self.wrap_present(v, k, total))
            }
        }
    }

    /// Reads the payload of a proven-present optional (the recorded type
    /// at the use is the inner type): the word at +8, or an interior
    /// pointer for multi-word payloads — consumers copy.
    fn payload_read(&mut self, opt: V, inner: &Type, span: Span) -> Result<V, Diagnostic> {
        let k = kind_of(inner, self.res, FUEL)
            .ok_or_else(|| unsupported("values of this type", span))?;
        let dst = self.fresh(*inner == Type::Float);
        self.insts.push(if k == Kind::Word {
            Inst::LoadAt {
                dst,
                base: opt,
                off: 8,
            }
        } else {
            Inst::LeaAt {
                dst,
                base: opt,
                off: 8,
            }
        });
        Ok(dst)
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
                // The slot's shape follows the declared type (ADR 0021);
                // the checker resolved the annotation — gate on that.
                let slot_ty = if ty.is_some() {
                    self.res
                        .let_types
                        .get(&stmt.span())
                        .ok_or_else(|| unsupported("bindings of this type", stmt.span()))?
                        .clone()
                } else {
                    self.ty(&value.span())
                        .cloned()
                        .ok_or_else(|| unsupported("bindings of this type", stmt.span()))?
                };
                let kind = kind_of(&slot_ty, self.res, FUEL)
                    .ok_or_else(|| unsupported("bindings of this type", stmt.span()))?;
                let v = self.expr_into(value, &slot_ty)?;
                let slot = if kind == Kind::Word {
                    let s = self.fresh(self.floats[v]);
                    self.insts.push(Inst::Copy(s, v));
                    s
                } else {
                    self.snapshot(v, kind.words())
                };
                let opt_inner = self.opt_inner_of(&slot_ty);
                self.scopes
                    .last_mut()
                    .expect("a scope is always open")
                    .insert(name.clone(), Binding { v: slot, opt_inner });
            }
            Stmt::Assign { target, value, .. } => match target {
                Expr::Ident(name, span) => {
                    let b = self
                        .lookup(name)
                        .ok_or_else(|| unsupported("this assignment target", *span))?;
                    if let Some(inner) = b.opt_inner {
                        // Opt-shaped slot: wrap the value in place.
                        let target = Type::Optional(Box::new(inner));
                        let words = kind_of(&target, self.res, FUEL)
                            .ok_or_else(|| unsupported("this assignment target", *span))?
                            .words();
                        let v = self.expr_into(value, &target)?;
                        self.insts.push(Inst::CopyW {
                            dst: b.v,
                            src: v,
                            words,
                        });
                    } else {
                        let kind = self.kind(value, *span)?;
                        let v = self.expr(value)?;
                        if kind == Kind::Word {
                            self.insts.push(Inst::Copy(b.v, v));
                        } else {
                            self.insts.push(Inst::CopyW {
                                dst: b.v,
                                src: v,
                                words: kind.words(),
                            });
                        }
                    }
                }
                // The oracle evaluates the value before the target.
                Expr::Index { base, index, span } => {
                    let loc = self.loc_of(*span);
                    let elem = self.elem_ty(base)?;
                    let ek = kind_of(&elem, self.res, FUEL)
                        .ok_or_else(|| unsupported("arrays of this element type", *span))?;
                    let val = self.expr_into(value, &elem)?;
                    // Snapshot at evaluation: the target's index
                    // expression may push and move the buffer this
                    // pointer aims into.
                    let val = if ek == Kind::Word {
                        val
                    } else {
                        self.snapshot(val, ek.words())
                    };
                    let arr = self.expr(base)?;
                    let idx = self.expr(index)?;
                    let agg = (ek != Kind::Word).then(|| ek.words());
                    self.insts.push(Inst::IndexSet {
                        arr,
                        idx,
                        val,
                        loc,
                        agg,
                    });
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
                    let target = slot.ty.clone();
                    let val = self.expr_into(value, &target)?;
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
                    let ret_ty = self.ret_ty.clone();
                    let v = self.expr_into(expr, &ret_ty)?;
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
                let elem = self.elem_ty(iterable)?;
                let ek = kind_of(&elem, self.res, FUEL)
                    .ok_or_else(|| unsupported("arrays of this element type", iterable.span()))?;
                let arr = self.expr(iterable)?;
                let i = self.const_word(0);
                let x = self.fresh(elem == Type::Float);
                if ek != Kind::Word {
                    self.insts.push(Inst::Temp {
                        dst: x,
                        words: ek.words(),
                    });
                }
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
                // The loop condition proves the bound; the check's trap
                // path is unreachable but keeps one Index shape.
                let loc = self.loc_of(iterable.span());
                if ek == Kind::Word {
                    self.insts.push(Inst::Index {
                        dst: x,
                        arr,
                        idx: i,
                        loc,
                        agg: None,
                    });
                } else {
                    // Interior pointer, then the per-step copy-out.
                    let p = self.fresh(false);
                    self.insts.push(Inst::Index {
                        dst: p,
                        arr,
                        idx: i,
                        loc,
                        agg: Some(ek.words()),
                    });
                    self.insts.push(Inst::CopyW {
                        dst: x,
                        src: p,
                        words: ek.words(),
                    });
                }
                let cont = self.fresh_label();
                let mut bindings = HashMap::new();
                bindings.insert(
                    name.clone(),
                    Binding {
                        v: x,
                        opt_inner: self.opt_inner_of(&elem),
                    },
                );
                if let Some(ix) = index {
                    bindings.insert(
                        ix.clone(),
                        Binding {
                            v: i,
                            opt_inner: None,
                        },
                    );
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
            // A bare `null` reaching plain expr() is a ref-optional's
            // handle 0; value-optional slots wrap it in `expr_into`.
            Expr::Null(_) => Ok(self.const_word(0)),
            // A literal's descriptor and bytes are both static — strings
            // are immutable, so every use shares one rodata object.
            Expr::Str(text, _) => {
                let sym = self.strings.intern(text);
                Ok(self.lea_sym(sym))
            }
            Expr::Ident(name, span) => {
                let b = self
                    .lookup(name)
                    .ok_or_else(|| unsupported("this name", *span))?;
                // Narrowing proved presence when the recorded type is the
                // payload type — read through the tag (ADR 0021).
                if let Some(inner) = &b.opt_inner
                    && !matches!(self.ty(span), Some(Type::Optional(_)))
                {
                    return self.payload_read(b.v, &inner.clone(), *span);
                }
                Ok(b.v)
            }
            // float(i) is one convert; int(f) is the checked form —
            // NaN and out-of-range report and exit 1 (ADR 0028);
            // string(x) renders through the shared builder (ADR 0029).
            Expr::Convert { to, arg, span, .. } => match to {
                Conv::Float => {
                    let v = self.expr(arg)?;
                    let dst = self.fresh(true);
                    self.insts.push(Inst::IntToFloat(dst, v));
                    Ok(dst)
                }
                Conv::Int => {
                    let v = self.expr(arg)?;
                    let loc = self.loc_of(*span);
                    let dst = self.fresh(false);
                    self.insts.push(Inst::FloatToInt { dst, src: v, loc });
                    Ok(dst)
                }
                Conv::Str => self.stringify(arg, *span),
            },
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
                span,
            } => self.coalesce(lhs, rhs, *span),
            Expr::Binary { op, lhs, rhs, span } => self.binary(*op, lhs, rhs, *span),
            Expr::Call { callee, args, span } => self.call(callee, args, *span),
            Expr::ArrayLit { elements, span } => {
                let elem = self.elem_ty(expr)?;
                let ek = kind_of(&elem, self.res, FUEL)
                    .ok_or_else(|| unsupported("arrays of this element type", *span))?;
                let stride = 8 * ek.words() as i64;
                // Header {len, cap, data*} plus buffer, per ADR 0014;
                // elements sit at a compile-time stride (ADR 0023).
                let c24 = self.const_word(24);
                let hdr = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: hdr,
                    sym: RT_MALLOC,
                    args: vec![c24],
                    varargs: false,
                });
                let size = self.const_word((stride * elements.len() as i64).max(8));
                let buf = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst: buf,
                    sym: RT_MALLOC,
                    args: vec![size],
                    varargs: false,
                });
                self.insts.push(Inst::StoreHdr {
                    hdr,
                    buf,
                    len: elements.len(),
                });
                // Store as each element evaluates — the oracle's copy
                // point; optional elements wrap here (the fits rule).
                for (slot, element) in elements.iter().enumerate() {
                    let val = self.expr_into(element, &elem)?;
                    if ek == Kind::Word {
                        self.insts.push(Inst::BufSet { buf, slot, val });
                    } else {
                        let p = self.lea_at(buf, stride * slot as i64);
                        self.insts.push(Inst::CopyW {
                            dst: p,
                            src: val,
                            words: ek.words(),
                        });
                    }
                }
                Ok(hdr)
            }
            Expr::Index { base, index, span } => {
                let loc = self.loc_of(*span);
                // The recorded type IS the element type — index
                // expressions are never narrowable places. Aggregates
                // (any width) read as interior pointers; consumers copy.
                let agg = match self.ty(span) {
                    None => None,
                    Some(t) => {
                        let k = kind_of(t, self.res, FUEL)
                            .ok_or_else(|| unsupported("arrays of this element type", *span))?;
                        (k != Kind::Word).then(|| k.words())
                    }
                };
                let arr = self.expr(base)?;
                let idx = self.expr(index)?;
                let dst = self.fresh(matches!(self.ty(span), Some(Type::Float)));
                self.insts.push(Inst::Index {
                    dst,
                    arr,
                    idx,
                    loc,
                    agg,
                });
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
                let slot_ty = slot.ty.clone();
                let float = matches!(slot_ty, Type::Float);
                let b = self.expr(base)?;
                if *optional {
                    return self.optional_field(b, base, &slot_ty, off, *span);
                }
                // Narrowing proved an optional field present when the
                // recorded type is the payload type (ADR 0021): read
                // through the tag.
                if let Some(inner) = self.opt_inner_of(&slot_ty)
                    && !matches!(self.ty(span), Some(Type::Optional(_)))
                {
                    let k = kind_of(&inner, self.res, FUEL)
                        .ok_or_else(|| unsupported("fields of this type", *span))?;
                    let r = self.fresh(inner == Type::Float);
                    self.insts.push(if k == Kind::Word {
                        Inst::LoadAt {
                            dst: r,
                            base: b,
                            off: off + 8,
                        }
                    } else {
                        Inst::LeaAt {
                            dst: r,
                            base: b,
                            off: off + 8,
                        }
                    });
                    return Ok(r);
                }
                if kind == Kind::Word {
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
                        sym: RT_MALLOC,
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
                    let ft = def.fields[i].1.clone();
                    let val = self.expr_into(value, &ft)?;
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
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            // Either side may be a tagged value optional; the other is
            // then a payload value or `null` (ADR 0021 decision 5).
            let l_opt = self.value_opt_inner(lhs);
            let r_opt = self.value_opt_inner(rhs);
            if l_opt.is_some() || r_opt.is_some() {
                return self.optional_eq(op, lhs, rhs, l_opt, r_opt, span);
            }
            if kind != Kind::Word {
                let lt = self
                    .ty(&lhs.span())
                    .cloned()
                    .ok_or_else(|| unsupported("values of this type", span))?;
                return self.aggregate_eq(op, &lt, kind, lhs, rhs, span);
            }
        }
        self.scalar_binary(op, lhs, rhs, span)
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
            sym: RT_MALLOC,
            args: vec![total],
            varargs: false,
        });
        let pa = self.load_at(l, 0);
        let d1 = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: d1,
            sym: RT_MEMCPY,
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
            sym: RT_MEMCPY,
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

    /// Scalar arithmetic and comparisons — IEEE via SSE for floats,
    /// wrapping two's complement for ints, with constant right operands
    /// strength-reduced (immediates, shift sequences, magic multiplies).
    fn scalar_binary(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Result<V, Diagnostic> {
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
        // Runtime divisors go through the checked form: divisor zero and
        // MIN/-1 report and exit instead of trapping (ADR 0022).
        if !float && matches!(op, BinOp::Div | BinOp::Rem) {
            let loc = self.loc_of(span);
            let v = self.fresh(false);
            self.insts.push(Inst::DivChecked {
                dst: v,
                lhs: l,
                rhs: r,
                rem: matches!(op, BinOp::Rem),
                loc,
            });
            return Ok(v);
        }
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

    fn lea_at(&mut self, base: V, off: i64) -> V {
        let dst = self.fresh(false);
        self.insts.push(Inst::LeaAt { dst, base, off });
        dst
    }

    /// `a ?? b` — a nullable handle keeps the left unless it is 0; a
    /// tagged optional selects on the tag (ADR 0021). The right side
    /// stays lazy. The result unwraps to the payload type when the rhs
    /// re-fills it, and keeps the optional shape when the rhs is
    /// optional or `null`.
    fn coalesce(&mut self, lhs: &Expr, rhs: &Expr, span: Span) -> Result<V, Diagnostic> {
        let lt = self
            .ty(&lhs.span())
            .cloned()
            .ok_or_else(|| unsupported("this coalesce", span))?;
        let Some(inner) = self.opt_inner_of(&lt) else {
            // Handle path: the null test composes from BinImm because
            // handles aren't 0/1 booleans.
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
            return Ok(v);
        };
        let ik = kind_of(&inner, self.res, FUEL)
            .ok_or_else(|| unsupported("values of this type", span))?;
        let l = self.expr(lhs)?;
        let tag = self.load_at(l, 0);
        let miss = self.fresh_label();
        let end = self.fresh_label();
        if matches!(self.ty(&span), Some(Type::Optional(_))) {
            // Optional result: select a pointer; a bare null rhs wraps.
            let v = self.fresh(false);
            self.insts.push(Inst::BrZero(tag, miss));
            self.insts.push(Inst::Copy(v, l));
            self.insts.push(Inst::Jmp(end));
            self.insts.push(Inst::Label(miss));
            let r = self.expr_into(rhs, &lt)?;
            self.insts.push(Inst::Copy(v, r));
            self.insts.push(Inst::Label(end));
            Ok(v)
        } else if ik == Kind::Word {
            let v = self.fresh(inner == Type::Float);
            self.insts.push(Inst::BrZero(tag, miss));
            self.insts.push(Inst::LoadAt {
                dst: v,
                base: l,
                off: 8,
            });
            self.insts.push(Inst::Jmp(end));
            self.insts.push(Inst::Label(miss));
            let r = self.expr(rhs)?;
            self.insts.push(Inst::Copy(v, r));
            self.insts.push(Inst::Label(end));
            Ok(v)
        } else {
            // Multi-word payload: copy either side into a result temp.
            let out = self.fresh(false);
            self.insts.push(Inst::Temp {
                dst: out,
                words: ik.words(),
            });
            self.insts.push(Inst::BrZero(tag, miss));
            let p = self.lea_at(l, 8);
            self.insts.push(Inst::CopyW {
                dst: out,
                src: p,
                words: ik.words(),
            });
            self.insts.push(Inst::Jmp(end));
            self.insts.push(Inst::Label(miss));
            let r = self.expr(rhs)?;
            self.insts.push(Inst::CopyW {
                dst: out,
                src: r,
                words: ik.words(),
            });
            self.insts.push(Inst::Label(end));
            Ok(out)
        }
    }

    /// `base?.field` — a null base short-circuits. Handle results stay a
    /// word (0 = null); value-typed and value-optional fields build a
    /// tagged optional — wrapped or copied whole (flattening, ADR 0021).
    fn optional_field(
        &mut self,
        b: V,
        base: &Expr,
        slot_ty: &Type,
        off: i64,
        span: Span,
    ) -> Result<V, Diagnostic> {
        // The base is a nullable handle (test the word) or a tagged
        // value optional (test the tag; its payload sits at +8).
        let (test, field_base, field_off) = match self.ty(&base.span()) {
            Some(Type::Optional(bi)) if !ref_shaped(bi, self.res) => {
                let tag = self.load_at(b, 0);
                (tag, b, off + 8)
            }
            _ => (b, b, off),
        };
        let result_ty = self
            .ty(&span)
            .cloned()
            .ok_or_else(|| unsupported("this field access", span))?;
        match self.opt_inner_of(&result_ty) {
            // Handle-shaped result: 0 stays the null.
            None => {
                let r = self.const_word(0);
                let end = self.fresh_label();
                self.insts.push(Inst::BrZero(test, end));
                self.insts.push(Inst::LoadAt {
                    dst: r,
                    base: field_base,
                    off: field_off,
                });
                self.insts.push(Inst::Label(end));
                Ok(r)
            }
            Some(inner) => {
                let ik = kind_of(&inner, self.res, FUEL)
                    .ok_or_else(|| unsupported("fields of this type", span))?;
                let total = 1 + ik.words();
                let out = self.null_optional(total);
                let end = self.fresh_label();
                self.insts.push(Inst::BrZero(test, end));
                if self.opt_inner_of(slot_ty).is_some() {
                    // Already-optional field: copy whole (flattening).
                    let src = self.lea_at(field_base, field_off);
                    self.insts.push(Inst::CopyW {
                        dst: out,
                        src,
                        words: total,
                    });
                } else {
                    // Value field: wrap {1, payload} in place.
                    let one = self.const_word(1);
                    self.insts.push(Inst::StoreAt {
                        base: out,
                        off: 0,
                        val: one,
                    });
                    if ik == Kind::Word {
                        let p = self.load_at(field_base, field_off);
                        self.insts.push(Inst::StoreAt {
                            base: out,
                            off: 8,
                            val: p,
                        });
                    } else {
                        let srcp = self.lea_at(field_base, field_off);
                        let dstp = self.lea_at(out, 8);
                        self.insts.push(Inst::CopyW {
                            dst: dstp,
                            src: srcp,
                            words: ik.words(),
                        });
                    }
                }
                self.insts.push(Inst::Label(end));
                Ok(out)
            }
        }
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
        // Multi-word arguments copy into private temps AT EVALUATION
        // TIME: the oracle copies as it evaluates, so a later argument
        // mutating the storage through an alias must not be visible.
        // The parameter type is the slot shape — optionals wrap here.
        let params = sig.params.clone();
        let mut arg_vregs = Vec::new();
        for (arg, pty) in args.iter().zip(&params) {
            let kind = kind_of(pty, self.res, FUEL)
                .ok_or_else(|| unsupported("calls with arguments of this type", span))?;
            let v = self.expr_into(arg, pty)?;
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
                let elem = self.elem_ty(array)?;
                let ek = kind_of(&elem, self.res, FUEL)
                    .ok_or_else(|| unsupported("arrays of this element type", value.span()))?;
                let arr = self.expr(array)?;
                let val = self.expr_into(value, &elem)?;
                if ek == Kind::Word {
                    let dst = self.fresh(false);
                    self.insts.push(Inst::CallRt {
                        dst,
                        sym: RT_PUSH,
                        args: vec![arr, val],
                        varargs: false,
                    });
                    return Ok(dst);
                }
                // Call-argument discipline: snapshot at evaluation —
                // also keeps push(xs, xs[0]) safe across the realloc.
                let snap = self.snapshot(val, ek.words());
                let stride = self.const_word(8 * ek.words() as i64);
                let dst = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst,
                    sym: RT_PUSH_N,
                    args: vec![arr, snap, stride],
                    varargs: false,
                });
                Ok(dst)
            }
            ("print", [value]) => {
                let ty = self
                    .ty(&value.span())
                    .cloned()
                    .ok_or_else(|| unsupported("printing this value", span))?;
                match &ty {
                    Type::Int => {
                        let v = self.expr(value)?;
                        Ok(self.print_int(v))
                    }
                    Type::Bool => {
                        let v = self.expr(value)?;
                        Ok(self.print_bool(v))
                    }
                    Type::Str => {
                        let v = self.expr(value)?;
                        Ok(self.print_str_desc(v))
                    }
                    // A bare null literal prints its text.
                    Type::Null => Ok(self.print_null()),
                    // A tagged optional prints its payload or `null`
                    // (ADR 0021 decision 6).
                    Type::Optional(inner) if !ref_shaped(inner, self.res) => match inner.as_ref() {
                        Type::Int | Type::Bool | Type::Str => {
                            let v = self.expr(value)?;
                            let tag = self.load_at(v, 0);
                            let miss = self.fresh_label();
                            let end = self.fresh_label();
                            self.insts.push(Inst::BrZero(tag, miss));
                            match inner.as_ref() {
                                Type::Int => {
                                    let p = self.load_at(v, 8);
                                    self.print_int(p);
                                }
                                Type::Bool => {
                                    let p = self.load_at(v, 8);
                                    self.print_bool(p);
                                }
                                Type::Str => {
                                    let d = self.lea_at(v, 8);
                                    self.print_str_desc(d);
                                }
                                _ => unreachable!(),
                            }
                            self.insts.push(Inst::Jmp(end));
                            self.insts.push(Inst::Label(miss));
                            self.print_null();
                            self.insts.push(Inst::Label(end));
                            Ok(self.const_word(0))
                        }
                        // Aggregate payloads (struct?): the show
                        // routine's tag wrapper handles null.
                        _ => self.print_aggregate(value, &ty, span),
                    },
                    // The runtime formatter (ADR 0027) appends to the
                    // builder; the value's bits ride an integer register.
                    Type::Float => {
                        let v = self.expr(value)?;
                        self.sb_reset();
                        let dst = self.fresh(false);
                        self.insts.push(Inst::CallRt {
                            dst,
                            sym: RT_FMT_F64,
                            args: vec![v],
                            varargs: false,
                        });
                        Ok(self.sb_print())
                    }
                    // A unit-typed call: evaluate for effects, print
                    // the oracle's literal text.
                    Type::Unit => {
                        self.expr(value)?;
                        let sym = self.strings.intern_cstr("unit");
                        let s = self.lea_sym(sym);
                        Ok(self.print_cstr(s))
                    }
                    _ => self.print_aggregate(value, &ty, span),
                }
            }
            _ => Err(unsupported(&format!("builtin '{name}'"), span)),
        }
    }

    /// Aggregates print through their monomorphized show routine
    /// (ADR 0025).
    fn print_aggregate(&mut self, value: &Expr, ty: &Type, span: Span) -> Result<V, Diagnostic> {
        kind_of(ty, self.res, FUEL)
            .ok_or_else(|| unsupported("printing values of this type", span))?;
        let v = self.expr(value)?;
        let name = self.printers.request(ty, self.res);
        self.sb_reset();
        let depth = self.const_word(DEPTH_BUDGET);
        let dst = self.fresh(false);
        self.insts.push(Inst::Call {
            dst,
            label: label_of(0, &name),
            args: vec![v, depth],
            sret: None,
        });
        Ok(self.sb_print())
    }

    /// Clears the shared text builder before its producers run
    /// (ADR 0029). Callers evaluate the argument first, so a print
    /// buried in it cannot interleave with this site's builder use.
    fn sb_reset(&mut self) {
        let h = self.lea_sym(SB_HDR.into());
        let z = self.const_word(0);
        self.insts.push(Inst::StoreAt {
            base: h,
            off: 0,
            val: z,
        });
    }

    /// print's tail for builder-rendered values: the accumulated bytes
    /// and the trailing newline in one `%.*s\n`.
    fn sb_print(&mut self) -> V {
        let h = self.lea_sym(SB_HDR.into());
        let len = self.load_at(h, 0);
        let ptr = self.load_at(h, 16);
        let fmt = self.lea_sym(FMT_STR.into());
        let dst = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst,
            sym: RT_PRINTF,
            args: vec![fmt, len, ptr],
            varargs: true,
        });
        self.const_word(0)
    }

    /// `string(x)` (ADR 0029): bool selects a static `"true"`/`"false"`
    /// descriptor; every other type renders through the shared builder
    /// and copies out to an exact-length heap string. Dispatch reads
    /// the recorded (possibly narrowed) type, like `print`.
    fn stringify(&mut self, arg: &Expr, span: Span) -> Result<V, Diagnostic> {
        let ty = self
            .ty(&arg.span())
            .cloned()
            .ok_or_else(|| unsupported("converting this value", span))?;
        let v = self.expr(arg)?;
        match &ty {
            // Implicit identity — a template's `${s}` (ADR 0030): the
            // value already IS its text.
            Type::Str => return Ok(v),
            Type::Bool => {
                let t = self.strings.intern(syntax::KW_TRUE);
                let r = self.lea_sym(t);
                let end = self.fresh_label();
                let isfalse = self.fresh(false);
                self.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: isfalse,
                    lhs: v,
                    imm: 0,
                });
                self.insts.push(Inst::BrZero(isfalse, end));
                let f = self.strings.intern(syntax::KW_FALSE);
                let fv = self.lea_sym(f);
                self.insts.push(Inst::Copy(r, fv));
                self.insts.push(Inst::Label(end));
                return Ok(r);
            }
            Type::Int => {
                self.sb_reset();
                let dst = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst,
                    sym: RT_SB_INT,
                    args: vec![v],
                    varargs: false,
                });
            }
            Type::Float => {
                self.sb_reset();
                let dst = self.fresh(false);
                self.insts.push(Inst::CallRt {
                    dst,
                    sym: RT_FMT_F64,
                    args: vec![v],
                    varargs: false,
                });
            }
            _ => {
                kind_of(&ty, self.res, FUEL)
                    .ok_or_else(|| unsupported("converting values of this type", span))?;
                let name = self.printers.request(&ty, self.res);
                self.sb_reset();
                let depth = self.const_word(DEPTH_BUDGET);
                let dst = self.fresh(false);
                self.insts.push(Inst::Call {
                    dst,
                    label: label_of(0, &name),
                    args: vec![v, depth],
                    sret: None,
                });
            }
        }
        Ok(self.sb_take())
    }

    /// Copies the builder's bytes into a fresh exact-length string —
    /// `string(x)`'s one allocation, a statement temp like concat's.
    fn sb_take(&mut self) -> V {
        let h = self.lea_sym(SB_HDR.into());
        let len = self.load_at(h, 0);
        let buf = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: buf,
            sym: RT_MALLOC,
            args: vec![len],
            varargs: false,
        });
        let src = self.load_at(h, 16);
        let d = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst: d,
            sym: RT_MEMCPY,
            args: vec![buf, src, len],
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
            val: len,
        });
        out
    }

    fn print_int(&mut self, v: V) -> V {
        let fmt = self.lea_sym(FMT_INT.into());
        let dst = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst,
            sym: RT_PRINTF,
            args: vec![fmt, v],
            varargs: true,
        });
        dst
    }

    fn print_bool(&mut self, v: V) -> V {
        let r = self.lea_sym(TRUE_S.into());
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
            sym: FALSE_S.into(),
        });
        self.insts.push(Inst::Copy(r, f));
        self.insts.push(Inst::Label(end));
        self.print_cstr(r)
    }

    fn print_null(&mut self) -> V {
        let s = self.lea_sym(NULL_S.into());
        self.print_cstr(s)
    }

    fn print_cstr(&mut self, s: V) -> V {
        let fmt = self.lea_sym(FMT_CSTR.into());
        let dst = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst,
            sym: RT_PRINTF,
            args: vec![fmt, s],
            varargs: true,
        });
        dst
    }

    // Length-carried, so %.*s with (len, ptr).
    fn print_str_desc(&mut self, v: V) -> V {
        let len = self.load_at(v, 8);
        let ptr = self.load_at(v, 0);
        let fmt = self.lea_sym(FMT_STR.into());
        let dst = self.fresh(false);
        self.insts.push(Inst::CallRt {
            dst,
            sym: RT_PRINTF,
            args: vec![fmt, len, ptr],
            varargs: true,
        });
        dst
    }
}
