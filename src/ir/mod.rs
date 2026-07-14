//! The backend (ADR 0018). A function's journey through this module:
//!
//! 1. [`lower`] walks the checked AST and flattens it into a list of
//!    simple instructions ([`Inst`]) over unlimited virtual registers —
//!    every operator, copy, call, and branch becomes one small step, in
//!    exactly the oracle's evaluation order.
//! 2. [`regalloc`] figures out when each virtual register is alive and
//!    assigns it a real CPU register (or a stack slot when they run out).
//! 3. [`emit`] prints one small block of AT&T assembly per instruction.
//!
//! [`layout`] answers "how big is this type and where do its fields
//! sit" for all three phases. Multi-word values (value structs, strings)
//! travel as pointers to frame or heap storage in ordinary word vregs,
//! copied exactly where the oracle copies: `let`, assignment, return,
//! each call argument at evaluation time, and equality's left operand.
//!
//! Runtime errors — division by zero, i64::MIN / -1, out-of-bounds —
//! report the oracle's message plus a source location on stderr and
//! exit 1 (ADR 0022), matching the interpreter's behavior class. The
//! differential harness still diffs only programs the oracle runs
//! cleanly; CLI tests pin the error-path parity.

mod emit;
mod layout;
mod lower;
mod regalloc;

use crate::ast::{BinOp, Function};
use crate::check::Resolutions;
use crate::codegen::Strings;
use crate::diagnostic::Diagnostic;
use crate::source::SourceMap;
use crate::span::Span;
use std::fmt;

/// A virtual register.
pub(crate) type V = usize;
/// A jump label within one function.
pub(crate) type Lbl = usize;

pub(crate) struct FunctionIr {
    name: String,
    module: usize,
    nparams: usize,
    vregs: usize,
    floats: Vec<bool>,
    insts: Vec<Inst>,
}

pub(crate) fn lower_function(
    f: &Function,
    module: usize,
    res: &Resolutions,
    strings: &mut Strings,
    map: &SourceMap,
) -> Result<FunctionIr, Diagnostic> {
    lower::lower(f, module, res, strings, map)
}

pub(crate) fn function(
    f: &Function,
    module: usize,
    res: &Resolutions,
    strings: &mut Strings,
    map: &SourceMap,
) -> Result<String, Diagnostic> {
    Ok(emit::emit(lower_function(f, module, res, strings, map)?))
}

fn unsupported(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}

pub(crate) fn cc(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "e",
        BinOp::Ne => "ne",
        BinOp::Lt => "l",
        BinOp::Le => "le",
        BinOp::Gt => "g",
        _ => "ge",
    }
}

// ---- Instructions ---------------------------------------------------------

pub(crate) enum Inst {
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
    /// Integer division with a runtime divisor: divisor-zero and
    /// MIN/-1 branch to the trap stubs before idiv (ADR 0022).
    DivChecked {
        dst: V,
        lhs: V,
        rhs: V,
        rem: bool,
        loc: String,
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
    /// violation reports and exits 1 via the OOB trap stub, ADR 0022).
    Index {
        dst: V,
        arr: V,
        idx: V,
        loc: String,
    },
    IndexSet {
        arr: V,
        idx: V,
        val: V,
        loc: String,
    },
    Ret(V),
    Jmp(Lbl),
    /// Falls through when `cond` is nonzero, jumps when zero.
    BrZero(V, Lbl),
    Label(Lbl),
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Eq => "eq",
        BinOp::Ne => "ne",
        BinOp::Lt => "lt",
        BinOp::Le => "le",
        BinOp::Gt => "gt",
        BinOp::Ge => "ge",
        BinOp::Coalesce => "coalesce",
    }
}

fn vreg_list(values: &[V]) -> String {
    values
        .iter()
        .map(|v| format!("v{v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inst::Const(dst, n) => write!(f, "v{dst} = const {n}"),
            Inst::Copy(dst, src) => write!(f, "v{dst} = copy v{src}"),
            Inst::Bin {
                op,
                float,
                dst,
                lhs,
                rhs,
            } => write!(
                f,
                "v{dst} = {}.{} v{lhs}, v{rhs}",
                op_name(*op),
                if *float { "float" } else { "word" }
            ),
            Inst::BinImm { op, dst, lhs, imm } => {
                write!(f, "v{dst} = {}.imm v{lhs}, {imm}", op_name(*op))
            }
            Inst::DivPow2 { dst, src, k } => {
                write!(f, "v{dst} = div.pow2 v{src}, {k}")
            }
            Inst::RemPow2 { dst, src, k } => {
                write!(f, "v{dst} = rem.pow2 v{src}, {k}")
            }
            Inst::DivMagic { dst, src, d } => {
                write!(f, "v{dst} = div.magic v{src}, {d}")
            }
            Inst::RemMagic { dst, src, d } => {
                write!(f, "v{dst} = rem.magic v{src}, {d}")
            }
            Inst::DivChecked {
                dst,
                lhs,
                rhs,
                rem,
                loc,
            } => {
                let name = if *rem { "rem" } else { "div" };
                write!(f, "v{dst} = {name}.checked v{lhs}, v{rhs} @ {loc}")
            }
            Inst::Neg(dst, src) => write!(f, "v{dst} = neg.word v{src}"),
            Inst::NegF(dst, src) => write!(f, "v{dst} = neg.float v{src}"),
            Inst::Not(dst, src) => write!(f, "v{dst} = not v{src}"),
            Inst::Call {
                dst,
                label,
                args,
                sret,
            } => {
                write!(f, "v{dst} = call {label}({})", vreg_list(args))?;
                if let Some(sret) = sret {
                    write!(f, ", sret v{sret}")?;
                }
                Ok(())
            }
            Inst::CallRt {
                dst,
                sym,
                args,
                varargs,
            } => {
                write!(f, "v{dst} = call_rt {sym}({})", vreg_list(args))?;
                if *varargs {
                    write!(f, ", varargs")?;
                }
                Ok(())
            }
            Inst::Temp { dst, words } => write!(f, "v{dst} = temp {words}w"),
            Inst::CopyW { dst, src, words } => {
                write!(f, "copy {words}w v{src} -> v{dst}")
            }
            Inst::LoadAt { dst, base, off } => {
                write!(f, "v{dst} = load v{base}{off:+}")
            }
            Inst::StoreAt { base, off, val } => {
                write!(f, "store v{val} -> v{base}{off:+}")
            }
            Inst::LeaAt { dst, base, off } => {
                write!(f, "v{dst} = lea v{base}{off:+}")
            }
            Inst::LeaSym { dst, sym } => write!(f, "v{dst} = lea {sym}"),
            Inst::StoreHdr { hdr, buf, len } => {
                write!(f, "store_header v{hdr}, buffer v{buf}, len {len}")
            }
            Inst::BufSet { buf, slot, val } => {
                write!(f, "store_buffer v{val} -> v{buf}[{slot}]")
            }
            Inst::Len(dst, arr) => write!(f, "v{dst} = len v{arr}"),
            Inst::Index { dst, arr, idx, loc } => {
                write!(f, "v{dst} = index v{arr}, v{idx} @ {loc}")
            }
            Inst::IndexSet { arr, idx, val, loc } => {
                write!(f, "index_set v{arr}, v{idx}, v{val} @ {loc}")
            }
            Inst::Ret(value) => write!(f, "ret v{value}"),
            Inst::Jmp(label) => write!(f, "jump L{label}"),
            Inst::BrZero(value, label) => write!(f, "br_zero v{value}, L{label}"),
            Inst::Label(label) => write!(f, "L{label}:"),
        }
    }
}

impl fmt::Display for FunctionIr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "fn {} [module {}] (params {}, vregs {}) {{",
            self.name, self.module, self.nparams, self.vregs
        )?;
        let float_vregs: Vec<String> = self
            .floats
            .iter()
            .enumerate()
            .filter(|(_, float)| **float)
            .map(|(v, _)| format!("v{v}"))
            .collect();
        if !float_vregs.is_empty() {
            writeln!(f, "  float vregs: {}", float_vregs.join(", "))?;
        }
        for inst in &self.insts {
            writeln!(f, "  {inst}")?;
        }
        write!(f, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_ir_display_covers_values_memory_calls_and_control_flow() {
        let ir = FunctionIr {
            name: "probe".to_string(),
            module: 2,
            nparams: 1,
            vregs: 5,
            floats: vec![false, false, false, false, false],
            insts: vec![
                Inst::Const(1, 7),
                Inst::Bin {
                    op: BinOp::Add,
                    float: false,
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                Inst::LoadAt {
                    dst: 3,
                    base: 2,
                    off: 8,
                },
                Inst::Call {
                    dst: 4,
                    label: "helper_1".to_string(),
                    args: vec![3],
                    sret: Some(2),
                },
                Inst::BrZero(4, 0),
                Inst::Label(0),
                Inst::Ret(4),
            ],
        };
        assert_eq!(
            ir.to_string(),
            "fn probe [module 2] (params 1, vregs 5) {\n\
             \x20\x20v1 = const 7\n\
             \x20\x20v2 = add.word v0, v1\n\
             \x20\x20v3 = load v2+8\n\
             \x20\x20v4 = call helper_1(v3), sret v2\n\
             \x20\x20br_zero v4, L0\n\
             \x20\x20L0:\n\
             \x20\x20ret v4\n\
             }"
        );
    }
}
