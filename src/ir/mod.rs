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
//! Compiled behavior on the idiv traps — division by zero and
//! i64::MIN / -1 — and on out-of-bounds (abort) is the deferred-trap
//! policy: the interpreter diagnoses them, and the differential harness
//! only diffs programs the interpreter runs cleanly.

mod emit;
mod layout;
mod lower;
mod regalloc;

pub use lower::function;

use crate::diagnostic::Diagnostic;
use crate::span::Span;

/// A virtual register.
pub(crate) type V = usize;
/// A jump label within one function.
pub(crate) type Lbl = usize;

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

use crate::ast::BinOp;

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
