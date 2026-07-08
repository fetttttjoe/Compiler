//! The backend (ADR 0018): every function lowers to a flat
//! virtual-register IR, gets linear-scan register allocation, and emits
//! AT&T assembly. Multi-word values (value structs, strings) travel as
//! pointers to frame or heap storage in ordinary word vregs, copied
//! exactly where the oracle copies: `let`, assignment, return, each call
//! argument at evaluation time, and equality's left operand. Locals are
//! mutable vregs (no phis); SSA arrives with the GVN slice (ADR 0016).
//!
//! Compiled behavior on the idiv traps — division by zero and
//! i64::MIN / -1 — and on out-of-bounds (abort) is the deferred-trap
//! policy: the interpreter diagnoses them, and the differential harness
//! only diffs programs the interpreter runs cleanly.

use crate::ast::{BinOp, Expr, Function, Stmt, TypeAnn, UnOp};
use crate::check::Resolutions;
use crate::codegen::{label_of, Strings};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::types::{StructType, Type};
use std::collections::HashMap;
use std::fmt::Write;

type V = usize;
type Lbl = usize;

/// Recursion bound for layout walks — a recursive value struct has
/// infinite size; its values can't exist, so hitting this is diagnostic.
const FUEL: usize = 64;

fn unsupported(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}

// ---- Value kinds ----------------------------------------------------------

/// The backend's view of a value: one word (scalars, handles), a str
/// (two-word fat pointer, content equality), or a value struct.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Word,
    Str,
    Struct { words: usize, no_memcmp: bool },
}

impl Kind {
    fn words(self) -> usize {
        match self {
            Kind::Word => 1,
            Kind::Str => 2,
            Kind::Struct { words, .. } => words,
        }
    }
}

/// A reference-shaped checker type: a handle where 0 means `null`, so a
/// `T?` of it is a nullable pointer for free (ADR 0009).
fn ref_shaped(t: &Type, res: &Resolutions) -> bool {
    match t {
        Type::Array(_) => true,
        Type::Struct(m, n) => res.structs[&(*m, n.clone())].by_ref,
        _ => false,
    }
}

/// A checker type's backend kind. `None` = not compilable yet (value
/// optionals, float printing aside) or infinite (recursive value struct).
fn kind_of(t: &Type, res: &Resolutions, fuel: usize) -> Option<Kind> {
    match t {
        Type::Int | Type::Bool | Type::Float => Some(Kind::Word),
        // An empty literal's unconstrained element ([]): a handle word.
        Type::Unknown => Some(Kind::Word),
        // A nullable handle is a word only if the handle itself is a
        // compilable word (an `int?[]?` must stay as gated as `int?`).
        Type::Optional(inner) => (ref_shaped(inner, res)
            && kind_of(inner, res, fuel.checked_sub(1)?)? == Kind::Word)
            .then_some(Kind::Word),
        // Elements must be single words (ADR 0014's stride seat); a
        // value-optional element would alias 0 with null.
        Type::Array(inner) => {
            (kind_of(inner, res, fuel.checked_sub(1)?)? == Kind::Word).then_some(Kind::Word)
        }
        Type::Str => Some(Kind::Str),
        Type::Struct(m, n) => {
            let def = &res.structs[&(*m, n.clone())];
            if def.by_ref {
                return Some(Kind::Word);
            }
            let next = fuel.checked_sub(1)?;
            let mut words = 0;
            let mut no_memcmp = false;
            for (_, ft) in &def.fields {
                no_memcmp |= matches!(ft, Type::Float);
                match kind_of(ft, res, next)? {
                    Kind::Word => words += 1,
                    Kind::Str => {
                        words += 2;
                        no_memcmp = true;
                    }
                    Kind::Struct {
                        words: w,
                        no_memcmp: n,
                    } => {
                        words += w;
                        no_memcmp |= n;
                    }
                }
            }
            Some(Kind::Struct { words, no_memcmp })
        }
        _ => None,
    }
}

/// Byte offset of field `index` in `def` — the sum of the sizes before
/// it (C-style declaration-order layout, ADR 0009).
fn offset_of(def: &StructType, index: usize, res: &Resolutions) -> Option<i64> {
    def.fields[..index].iter().try_fold(0, |sum, (_, ft)| {
        Some(sum + 8 * kind_of(ft, res, FUEL)?.words() as i64)
    })
}

// ---- Instructions ---------------------------------------------------------

enum Inst {
    Const(V, i64),
    Copy(V, V),
    Bin {
        op: BinOp,
        float: bool,
        dst: V,
        lhs: V,
        rhs: V,
    },
    BinImm {
        op: BinOp,
        dst: V,
        lhs: V,
        imm: i64,
    },
    DivPow2 {
        dst: V,
        src: V,
        k: u32,
    },
    RemPow2 {
        dst: V,
        src: V,
        k: u32,
    },
    DivMagic {
        dst: V,
        src: V,
        d: i64,
    },
    RemMagic {
        dst: V,
        src: V,
        d: i64,
    },
    Neg(V, V),
    NegF(V, V),
    Not(V, V),
    Call {
        dst: V,
        label: String,
        args: Vec<V>,
        /// Struct-returning callees take this destination pointer as a
        /// hidden first argument.
        sret: Option<V>,
    },
    /// A runtime/libc call: a clobber point like Call. `varargs` zeroes
    /// %al (the SysV vector-register count) for printf.
    CallRt {
        dst: V,
        sym: &'static str,
        args: Vec<V>,
        varargs: bool,
    },
    /// dst = pointer to a static per-site frame slot of `words` words.
    Temp {
        dst: V,
        words: usize,
    },
    /// rep movsq: `words` from *src to *dst (the pointer vregs stay
    /// intact; only reserved %rcx/%rsi/%rdi scratch is touched).
    CopyW {
        dst: V,
        src: V,
        words: usize,
    },
    LoadAt {
        dst: V,
        base: V,
        off: i64,
    },
    StoreAt {
        base: V,
        off: i64,
        val: V,
    },
    LeaAt {
        dst: V,
        base: V,
        off: i64,
    },
    /// dst = &sym (RIP-relative rodata: formats, string descriptors).
    LeaSym {
        dst: V,
        sym: String,
    },
    /// Array header init: buf into 16(hdr), len into 0/8(hdr).
    StoreHdr {
        hdr: V,
        buf: V,
        len: usize,
    },
    /// Literal element store at a constant slot: val into 8*i(buf).
    BufSet {
        buf: V,
        slot: usize,
        val: V,
    },
    Len(V, V),
    /// Bounds-checked element read/write (ADR 0008's runtime check;
    /// violation aborts — the deferred-trap policy).
    Index {
        dst: V,
        arr: V,
        idx: V,
    },
    IndexSet {
        arr: V,
        idx: V,
        val: V,
    },
    Ret(V),
    Jmp(Lbl),
    /// Falls through when `cond` is nonzero, jumps when zero.
    BrZero(V, Lbl),
    Label(Lbl),
}

// ---- Lowering -------------------------------------------------------------

struct Lowerer<'a> {
    res: &'a Resolutions,
    strings: &'a mut Strings,
    module: usize,
    insts: Vec<Inst>,
    scopes: Vec<HashMap<String, V>>,
    vregs: usize,
    /// Parallel to vregs: floats allocate from the XMM pool.
    floats: Vec<bool>,
    labels: usize,
    /// The hidden destination pointer of a struct-returning function.
    sret: Option<V>,
    ret_words: usize,
}

/// Compiles one function to assembly text.
pub fn function(
    f: &Function,
    module: usize,
    res: &Resolutions,
    strings: &mut Strings,
) -> Result<String, Diagnostic> {
    let sig = res.sigs[&(module, f.name.clone())].clone();
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
    let nregs = lo.vregs;
    for stmt in &f.body {
        lo.stmt(stmt)?;
    }
    // Fall-through for unit functions; value functions always return
    // (checker-proven) so the extra ret is dead.
    let zero = lo.const_word(0);
    lo.insts.push(Inst::Ret(zero));

    Ok(emit(&f.name, module, nregs, lo))
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
                if let Some(ann) = ty {
                    if self.ann_ok(ann).is_none() {
                        return Err(unsupported("bindings of this type", stmt.span()));
                    }
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
                let mut bindings = HashMap::new();
                bindings.insert(name.clone(), x);
                if let Some(ix) = index {
                    bindings.insert(ix.clone(), i);
                }
                self.scopes.push(bindings);
                let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
                self.scopes.pop();
                result?;
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

    /// The `TypeAnn` gate for `let` annotations, resolved through this
    /// module's names (mirrors `kind_of`; `None` = not compilable).
    fn ann_ok(&self, ty: &TypeAnn) -> Option<Kind> {
        match ty {
            TypeAnn::Int | TypeAnn::Bool | TypeAnn::Float => Some(Kind::Word),
            TypeAnn::Str => Some(Kind::Str),
            TypeAnn::Array(inner) => (self.ann_ok(inner)? == Kind::Word).then_some(Kind::Word),
            TypeAnn::Optional(inner) => match inner.as_ref() {
                TypeAnn::Named(n) => self.res.ref_structs[self.module]
                    .contains(n)
                    .then_some(Kind::Word),
                TypeAnn::Array(_) => self.ann_ok(inner),
                _ => None,
            },
            TypeAnn::Named(n) => {
                let key = self.res.types[self.module].get(n)?;
                kind_of(&Type::Struct(key.0, key.1.clone()), self.res, FUEL)
            }
        }
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
                let id = self.strings.intern(text);
                Ok(self.lea_sym(format!(".Lsd{id}")))
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
                let def = self.res.structs[&key].clone();
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

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Result<V, Diagnostic> {
        let kind = self.kind(lhs, span)?;
        // Concatenation: the one explicitly allocating string operation
        // (ADR 0013) — new buffer, both byte runs copied, fresh
        // descriptor in a statement temp.
        if kind == Kind::Str && matches!(op, BinOp::Add) {
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
            return Ok(out);
        }
        if matches!(op, BinOp::Eq | BinOp::Ne) && kind != Kind::Word {
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
            return Ok(eq);
        }
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
                let pow2 = *n >= 2 && (*n & (*n - 1)) == 0;
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
            if let Expr::Int(n, _) = lhs {
                if matches!(op, BinOp::Add | BinOp::Mul) && i32::try_from(*n).is_ok() {
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
        let sig = self.res.sigs[&key].clone();
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

// ---- Register allocation --------------------------------------------------

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
    Spill(i64),
}

struct Interval {
    vreg: V,
    start: usize,
    end: usize,
    crosses_call: bool,
}

fn uses_defs(inst: &Inst) -> (Vec<V>, Option<V>) {
    match inst {
        Inst::Const(d, _) => (vec![], Some(*d)),
        Inst::Copy(d, s) => (vec![*s], Some(*d)),
        Inst::Bin { dst, lhs, rhs, .. } => (vec![*lhs, *rhs], Some(*dst)),
        Inst::BinImm { dst, lhs, .. } => (vec![*lhs], Some(*dst)),
        Inst::DivPow2 { dst, src, .. }
        | Inst::RemPow2 { dst, src, .. }
        | Inst::DivMagic { dst, src, .. }
        | Inst::RemMagic { dst, src, .. } => (vec![*src], Some(*dst)),
        Inst::Neg(d, s) | Inst::NegF(d, s) | Inst::Not(d, s) => (vec![*s], Some(*d)),
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
        Inst::Index { dst, arr, idx } => (vec![*arr, *idx], Some(*dst)),
        Inst::IndexSet { arr, idx, val } => (vec![*arr, *idx, *val], None),
        Inst::Ret(v) => (vec![*v], None),
        Inst::BrZero(v, _) => (vec![*v], None),
        Inst::Jmp(_) | Inst::Label(_) => (vec![], None),
    }
}

/// Live intervals by iterative backward dataflow over the flat list —
/// bitset per instruction, so machine-generated functions stay fast.
fn intervals(insts: &[Inst], vregs: usize) -> Vec<Interval> {
    let words = vregs.div_ceil(64);
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

    let mut live_in: Vec<Vec<u64>> = vec![vec![0; words]; insts.len()];
    let mut out = vec![0u64; words];
    let mut changed = true;
    while changed {
        changed = false;
        for i in (0..insts.len()).rev() {
            out.iter_mut().for_each(|w| *w = 0);
            for s in succs(i) {
                for (o, w) in out.iter_mut().zip(&live_in[s]) {
                    *o |= w;
                }
            }
            let (uses, def) = uses_defs(&insts[i]);
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
    for (i, inst) in insts.iter().enumerate() {
        let (uses, def) = uses_defs(inst);
        for v in uses.into_iter().chain(def) {
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
fn allocate(
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

// ---- Emission -------------------------------------------------------------

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

fn emit(name: &str, module: usize, nparams: usize, lo: Lowerer) -> String {
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
                        "\tcmpq ${imm}, {}\n\tset{cc} %al\n\tmovzbq %al, %rax\n\tmovq %rax, {}",
                        at(*lhs),
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
