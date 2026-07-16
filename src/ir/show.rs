//! Monomorphized text routines for aggregates (ADR 0025/0029): one IR
//! function per rendered type, appending to the shared builder and
//! mirroring `interpreter/render.rs` — the normative text — byte for
//! byte: name-sorted fields, raw strings, and the depth budget where a
//! refstruct hop costs a level. `print` and `string()` both consume.

use super::layout::{FUEL, Kind, kind_of, offset_of, ref_shaped};
use super::{FunctionIr, Inst, Lbl, V};
use crate::ast::BinOp;
use crate::check::Resolutions;
use crate::codegen::{RT_FMT_F64, RT_SB_APPEND, RT_SB_INT, Strings, label_of};
use crate::types::Type;
use std::collections::HashMap;

/// The oracle's display budget (render.rs `display`): parity needs the
/// SAME depth.
pub(crate) const DEPTH_BUDGET: i64 = 8;

/// The show-routine registry: `print` and `string()` sites request a
/// label, routines generate at the end of compilation — transitively
/// over field and element types, memoized per type.
#[derive(Default)]
pub(crate) struct Printers {
    labels: HashMap<String, String>,
    pending: Vec<(String, Type)>,
}

impl Printers {
    /// The routine name for `t`. Ref-shaped optionals share the inner
    /// type's routine — handle routines absorb null. `Unknown` (an
    /// empty literal's element) prints as int: unreachable at runtime,
    /// the buffer is empty. The dot in the name cannot appear in a
    /// user identifier, so labels never collide.
    pub(crate) fn request(&mut self, t: &Type, res: &Resolutions) -> String {
        let t = match t {
            Type::Optional(inner) if ref_shaped(inner, res) => (**inner).clone(),
            Type::Unknown => Type::Int,
            other => other.clone(),
        };
        let key = format!("{t:?}");
        if let Some(name) = self.labels.get(&key) {
            return name.clone();
        }
        let name = format!("ys.show.{}", self.labels.len());
        self.labels.insert(key, name.clone());
        self.pending.push((name.clone(), t));
        name
    }

    /// Builds every pending routine; children enqueue transitively and
    /// the memo makes recursive types (cyclic refstructs) terminate.
    pub(crate) fn build(mut self, res: &Resolutions, strings: &mut Strings) -> Vec<FunctionIr> {
        let mut out = Vec::new();
        while let Some((name, t)) = self.pending.pop() {
            out.push(routine(name, &t, &mut self, res, strings));
        }
        out
    }
}

/// Every routine's two fixed params: the value (word kinds) or a
/// pointer to it (multi-word kinds), and the remaining depth.
const X: V = 0;
const D: V = 1;

/// Instruction builder for one routine body.
struct B<'a> {
    insts: Vec<Inst>,
    vregs: usize,
    labels: usize,
    strings: &'a mut Strings,
}

impl B<'_> {
    fn fresh(&mut self) -> V {
        self.vregs += 1;
        self.vregs - 1
    }

    fn label(&mut self) -> Lbl {
        self.labels += 1;
        self.labels - 1
    }

    fn konst(&mut self, n: i64) -> V {
        let v = self.fresh();
        self.insts.push(Inst::Const(v, n));
        v
    }

    /// Appends a fixed fragment to the builder.
    fn piece(&mut self, text: &str) {
        let sym = self.strings.intern_cstr(text);
        let p = self.fresh();
        self.insts.push(Inst::LeaSym { dst: p, sym });
        let n = self.konst(text.len() as i64);
        self.append(p, n);
    }

    fn append(&mut self, ptr: V, len: V) {
        let dst = self.fresh();
        self.insts.push(Inst::CallRt {
            dst,
            sym: RT_SB_APPEND,
            args: vec![ptr, len],
            varargs: false,
        });
    }

    fn load(&mut self, base: V, off: i64) -> V {
        let dst = self.fresh();
        self.insts.push(Inst::LoadAt { dst, base, off });
        dst
    }

    fn ret(&mut self) {
        let z = self.konst(0);
        self.insts.push(Inst::Ret(z));
    }

    /// `v == imm` → append `...` and return; the depth floor and the
    /// refstruct hop test.
    fn eq_ret_ellipsis(&mut self, v: V, imm: i64) {
        let c = self.fresh();
        self.insts.push(Inst::BinImm {
            op: BinOp::Eq,
            dst: c,
            lhs: v,
            imm,
        });
        let past = self.label();
        self.insts.push(Inst::BrZero(c, past));
        self.piece("...");
        self.ret();
        self.insts.push(Inst::Label(past));
    }

    /// `handle == 0` → append `null` and return; ref-shaped `T?` shares
    /// `T`'s routine, and non-null handles never trigger it.
    fn null_handle_ret(&mut self) {
        let c = self.fresh();
        self.insts.push(Inst::BinImm {
            op: BinOp::Eq,
            dst: c,
            lhs: X,
            imm: 0,
        });
        let past = self.label();
        self.insts.push(Inst::BrZero(c, past));
        self.piece("null");
        self.ret();
        self.insts.push(Inst::Label(past));
    }

    fn sub(&mut self, v: V, imm: i64) -> V {
        let dst = self.fresh();
        self.insts.push(Inst::BinImm {
            op: BinOp::Sub,
            dst,
            lhs: v,
            imm,
        });
        dst
    }

    /// The child value at base+off, as it travels: word kinds load,
    /// multi-word kinds pass an interior pointer.
    fn child(&mut self, base: V, off: i64, kind: Kind) -> V {
        if kind == Kind::Word {
            self.load(base, off)
        } else {
            let dst = self.fresh();
            self.insts.push(Inst::LeaAt { dst, base, off });
            dst
        }
    }

    fn show(&mut self, name: &str, arg: V, depth: V) {
        let dst = self.fresh();
        self.insts.push(Inst::Call {
            dst,
            label: label_of(0, name),
            args: vec![arg, depth],
            sret: None,
        });
    }
}

fn routine(
    name: String,
    t: &Type,
    printers: &mut Printers,
    res: &Resolutions,
    strings: &mut Strings,
) -> FunctionIr {
    let mut b = B {
        insts: Vec::new(),
        vregs: 2,
        labels: 0,
        strings,
    };
    // display_depth's entry check: depth 0 renders anything as "...".
    b.eq_ret_ellipsis(D, 0);
    match t {
        Type::Int => {
            let dst = b.fresh();
            b.insts.push(Inst::CallRt {
                dst,
                sym: RT_SB_INT,
                args: vec![X],
                varargs: false,
            });
        }
        // The runtime formatter (ADR 0027); fields load as words, so
        // the bits already ride an integer vreg.
        Type::Float => {
            let dst = b.fresh();
            b.insts.push(Inst::CallRt {
                dst,
                sym: RT_FMT_F64,
                args: vec![X],
                varargs: false,
            });
        }
        Type::Bool => {
            let c = b.fresh();
            b.insts.push(Inst::BinImm {
                op: BinOp::Eq,
                dst: c,
                lhs: X,
                imm: 0,
            });
            let is_true = b.label();
            let end = b.label();
            b.insts.push(Inst::BrZero(c, is_true));
            b.piece("false");
            b.insts.push(Inst::Jmp(end));
            b.insts.push(Inst::Label(is_true));
            b.piece("true");
            b.insts.push(Inst::Label(end));
        }
        Type::Str => {
            let ptr = b.load(X, 0);
            let len = b.load(X, 8);
            b.append(ptr, len);
        }
        // Opaque handle (ADR 0031): constant text — an address could
        // never match the oracle.
        Type::File => b.piece("file"),
        // A declared error code (ADR 0034): `error.Name` selected by
        // code.
        Type::ErrCode => err_chain(&mut b, res, X),
        // `T!` (ADR 0034): the tag decides — 0 renders the payload at
        // the same depth (stored unwrapped in the oracle, like
        // optionals), ≥2 renders the code's name.
        Type::ErrUnion(inner) => {
            let tag = b.load(X, 0);
            let is_value = b.label();
            let end = b.label();
            b.insts.push(Inst::BrZero(tag, is_value));
            err_chain(&mut b, res, tag);
            b.insts.push(Inst::Jmp(end));
            b.insts.push(Inst::Label(is_value));
            let k = kind_of(inner, res, FUEL).expect("printable payload");
            let v = b.child(X, 8, k);
            let child = printers.request(inner, res);
            b.show(&child, v, D);
            b.insts.push(Inst::Label(end));
        }
        // Value-shaped optional (request normalized the ref-shaped
        // ones away): the tag decides. The interpreter stores payloads
        // unwrapped, so the payload renders at the SAME depth.
        Type::Optional(inner) => {
            let tag = b.load(X, 0);
            let is_null = b.label();
            let end = b.label();
            b.insts.push(Inst::BrZero(tag, is_null));
            let k = kind_of(inner, res, FUEL).expect("printable payload");
            let v = b.child(X, 8, k);
            let child = printers.request(inner, res);
            b.show(&child, v, D);
            b.insts.push(Inst::Jmp(end));
            b.insts.push(Inst::Label(is_null));
            b.piece("null");
            b.insts.push(Inst::Label(end));
        }
        Type::Array(inner) => {
            b.null_handle_ret();
            b.piece("[");
            let n = b.fresh();
            b.insts.push(Inst::Len(n, X));
            let data = b.load(X, 16);
            let dm = b.sub(D, 1);
            let ek = kind_of(inner, res, FUEL).expect("printable element");
            let child = printers.request(inner, res);
            let i = b.konst(0);
            let top = b.label();
            let end = b.label();
            b.insts.push(Inst::Label(top));
            let c = b.fresh();
            b.insts.push(Inst::Bin {
                op: BinOp::Lt,
                float: false,
                dst: c,
                lhs: i,
                rhs: n,
            });
            b.insts.push(Inst::BrZero(c, end));
            let first = b.fresh();
            b.insts.push(Inst::BinImm {
                op: BinOp::Eq,
                dst: first,
                lhs: i,
                imm: 0,
            });
            let comma = b.label();
            let elem = b.label();
            b.insts.push(Inst::BrZero(first, comma));
            b.insts.push(Inst::Jmp(elem));
            b.insts.push(Inst::Label(comma));
            b.piece(", ");
            b.insts.push(Inst::Label(elem));
            let off = b.fresh();
            b.insts.push(Inst::BinImm {
                op: BinOp::Mul,
                dst: off,
                lhs: i,
                imm: 8 * ek.words() as i64,
            });
            let addr = b.fresh();
            b.insts.push(Inst::Bin {
                op: BinOp::Add,
                float: false,
                dst: addr,
                lhs: data,
                rhs: off,
            });
            let v = if ek == Kind::Word {
                b.load(addr, 0)
            } else {
                addr
            };
            b.show(&child, v, dm);
            b.insts.push(Inst::BinImm {
                op: BinOp::Add,
                dst: i,
                lhs: i,
                imm: 1,
            });
            b.insts.push(Inst::Jmp(top));
            b.insts.push(Inst::Label(end));
            b.piece("]");
        }
        // `Circle(1.5)` / `Ready` (ADR 0036): a tag chain into the
        // live variant's name and payloads, children one level deeper
        // — the oracle's `display_variant`, byte for byte.
        Type::Enum(m, ename) => {
            let def = res.enums[&(*m, ename.clone())].clone();
            let dm = b.sub(D, 1);
            let tag = b.load(X, 0);
            let end = b.label();
            for (idx, (vname, payloads)) in def.variants.iter().enumerate() {
                let c = b.fresh();
                b.insts.push(Inst::BinImm {
                    op: BinOp::Eq,
                    dst: c,
                    lhs: tag,
                    imm: idx as i64,
                });
                let next = b.label();
                b.insts.push(Inst::BrZero(c, next));
                b.piece(vname);
                if !payloads.is_empty() {
                    b.piece("(");
                    let mut off = 8i64;
                    for (k, pt) in payloads.iter().enumerate() {
                        if k > 0 {
                            b.piece(", ");
                        }
                        let pk = kind_of(pt, res, FUEL).expect("printable payload");
                        let v = b.child(X, off, pk);
                        let child = printers.request(pt, res);
                        b.show(&child, v, dm);
                        off += 8 * pk.words() as i64;
                    }
                    b.piece(")");
                }
                b.insts.push(Inst::Jmp(end));
                b.insts.push(Inst::Label(next));
            }
            b.insts.push(Inst::Label(end));
        }
        Type::Struct(m, sname) => {
            let def = &res.structs[&(*m, sname.clone())];
            // A handle hop costs a level (render.rs): null absorb, the
            // depth-1 test, children at depth-2. Value structs render
            // children at depth-1.
            let dm = if def.by_ref {
                b.null_handle_ret();
                b.eq_ret_ellipsis(D, 1);
                b.sub(D, 2)
            } else {
                b.sub(D, 1)
            };
            // Render order sorts by field name — interpreter storage
            // order, observable spec — while offsets stay declaration
            // order (ADR 0009 layout).
            let mut order: Vec<usize> = (0..def.fields.len()).collect();
            order.sort_by_key(|i| def.fields[*i].0.clone());
            // Instance names display without module qualifiers
            // (ADR 0035) — the oracle's text, byte for byte.
            b.piece(&format!("{} {{ ", crate::types::pretty(sname)));
            for (k, idx) in order.iter().enumerate() {
                let (fname, ft) = &def.fields[*idx];
                let sep = if k == 0 {
                    format!("{fname}: ")
                } else {
                    format!(", {fname}: ")
                };
                b.piece(&sep);
                let off = offset_of(def, *idx, res).expect("printable layout");
                let fk = kind_of(ft, res, FUEL).expect("printable field");
                let v = b.child(X, off, fk);
                let child = printers.request(ft, res);
                b.show(&child, v, dm);
            }
            b.piece(" }");
        }
        other => unreachable!("no show routine for {other:?}"),
    }
    b.ret();
    finish(b, name)
}

/// Appends `error.Name` for the code in `v` — a linear compare chain
/// over the declared errors, codes from 2 in declaration order
/// (ADR 0034). Shared by the bare `error` routine and the `T!` error
/// branch.
// ponytail: linear chain; a code-indexed .rodata table if programs
// ever declare enough errors to feel it.
fn err_chain(b: &mut B, res: &Resolutions, v: V) {
    let end = b.label();
    for (i, ename) in res.error_names.iter().enumerate() {
        let c = b.fresh();
        b.insts.push(Inst::BinImm {
            op: BinOp::Eq,
            dst: c,
            lhs: v,
            imm: (i + 2) as i64,
        });
        let next = b.label();
        b.insts.push(Inst::BrZero(c, next));
        b.piece(&format!("error.{ename}"));
        b.insts.push(Inst::Jmp(end));
        b.insts.push(Inst::Label(next));
    }
    b.insts.push(Inst::Label(end));
}

fn finish(b: B, name: String) -> FunctionIr {
    let B { insts, vregs, .. } = b;
    FunctionIr {
        name,
        module: 0,
        nparams: 2,
        vregs,
        floats: vec![false; vregs],
        insts,
    }
}
