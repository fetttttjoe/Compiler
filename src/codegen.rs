//! The backend driver (ADR 0018): every function compiles through the
//! IR (src/ir.rs); this module owns program assembly — symbol naming,
//! the string table, the in-assembly runtime, and read-only data.
//!
//! Standing obligation carried from the slices: the system cc links PIE
//! by default, so data symbols are RIP-relative and descriptors needing
//! load-time relocations live in .data.rel.ro.

use crate::ast::{Function, Item, TypeAnn};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::source::SourceMap;
use crate::syntax;
use std::collections::HashMap;
use std::fmt::Write;

// ---- Symbol inventory ------------------------------------------------
// Every runtime symbol and read-only-data label the emitted assembly
// references, named once: the definitions below and every use in ir/
// share these constants — never a re-spelled literal (the lexer's
// syntax.rs discipline, applied to the backend).

/// The in-assembly array-push runtime routines: `ys_push` stores one
/// word from a register; `ys_push_n` memcpys a stride's worth of bytes
/// from a pointer (multi-word elements, ADR 0023).
pub(crate) const RT_PUSH: &str = "ys_push";
pub(crate) const RT_PUSH_N: &str = "ys_push_n";
pub(crate) const RT_PRINTF: &str = "printf@PLT";
pub(crate) const RT_MALLOC: &str = "malloc@PLT";
pub(crate) const RT_REALLOC: &str = "realloc@PLT";
pub(crate) const RT_MEMCPY: &str = "memcpy@PLT";
pub(crate) const RT_MEMCMP: &str = "memcmp@PLT";
pub(crate) const RT_FMOD: &str = "fmod@PLT";
pub(crate) const RT_DPRINTF: &str = "dprintf@PLT";
/// The float formatter (ADR 0027) and the libc pieces only it uses.
pub(crate) const RT_FMT_F64: &str = "ys_fmt_f64";
pub(crate) const RT_SNPRINTF: &str = "snprintf@PLT";
pub(crate) const RT_STRTOD: &str = "strtod@PLT";
// abort left the inventory with ADR 0022: traps report and exit 1.
pub(crate) const RT_EXIT: &str = "exit@PLT";

/// Trap stubs (ADR 0022): print a runtime diagnostic and exit 1.
pub(crate) const TRAP_DIV0: &str = "ys_trap_div0";
pub(crate) const TRAP_OVERFLOW: &str = "ys_trap_overflow";
pub(crate) const TRAP_OOB: &str = "ys_trap_oob";
pub(crate) const TRAP_F2I: &str = "ys_trap_f2i";

/// printf formats and fixed strings for `print`. The `_RAW` variants
/// carry no newline — the show routines print fragments (ADR 0025).
pub(crate) const FMT_INT: &str = ".Lfmt_int";
pub(crate) const FMT_INT_RAW: &str = ".Lfmt_int_raw";
pub(crate) const FMT_STR_RAW: &str = ".Lfmt_str_raw";
pub(crate) const FMT_CSTR: &str = ".Lfmt_cstr";
pub(crate) const FMT_STR: &str = ".Lfmt_str";
pub(crate) const TRUE_S: &str = ".Ltrue_s";
pub(crate) const FALSE_S: &str = ".Lfalse_s";
pub(crate) const NULL_S: &str = ".Lnull_s";
pub(crate) const FMT_TRAP: &str = ".Lfmt_trap";
pub(crate) const FMT_TRAP_OOB: &str = ".Lfmt_trap_oob";
pub(crate) const MSG_DIV0: &str = ".Lmsg_div0";
pub(crate) const MSG_OVERFLOW: &str = ".Lmsg_overflow";
pub(crate) const MSG_F2I: &str = ".Lmsg_f2i";

/// The assembly symbol for a function: the entry `main` keeps its name
/// (the C runtime calls it); everything else is suffixed with its module
/// index, which decodes uniquely (the suffix after the last underscore).
pub(crate) fn label_of(module: usize, name: &str) -> String {
    if module == 0 && name == syntax::ENTRY_FN {
        name.to_string()
    } else {
        format!("{name}_{module}")
    }
}

/// The program's interned string literals: raw bytes in .rodata (no
/// terminator; emitted as .byte lists to dodge escaping pitfalls) and
/// aligned `{ptr, len}` descriptors in .data.rel.ro (their address field
/// needs a load-time relocation — .rodata would mean TEXTREL in a PIE).
#[derive(Default)]
pub(crate) struct Strings {
    bytes: String,
    descriptors: String,
    ids: HashMap<String, usize>,
    locs: HashMap<String, usize>,
}

impl Strings {
    /// Returns the descriptor's symbol — the label format lives only here.
    pub(crate) fn intern(&mut self, text: &str) -> String {
        if let Some(&id) = self.ids.get(text) {
            return format!(".Lsd{id}");
        }
        let id = self.ids.len();
        let _ = writeln!(self.bytes, ".Lsb{id}:");
        for chunk in text.as_bytes().chunks(16) {
            let bytes: Vec<String> = chunk.iter().map(|b| b.to_string()).collect();
            let _ = writeln!(self.bytes, "\t.byte {}", bytes.join(","));
        }
        let _ = writeln!(
            self.descriptors,
            "\t.balign 8\n.Lsd{id}:\n\t.quad .Lsb{id}\n\t.quad {}",
            text.len()
        );
        self.ids.insert(text.to_string(), id);
        format!(".Lsd{id}")
    }

    /// Interns a NUL-terminated C string — trap locations (ADR 0022)
    /// and print fragments (ADR 0025); returns its label. Deduplicated
    /// per program.
    pub(crate) fn intern_cstr(&mut self, text: &str) -> String {
        if let Some(&id) = self.locs.get(text) {
            return format!(".Lloc{id}");
        }
        let id = self.locs.len();
        let _ = writeln!(self.bytes, ".Lloc{id}:");
        for chunk in text.as_bytes().chunks(16) {
            let bytes: Vec<String> = chunk.iter().map(|b| b.to_string()).collect();
            let _ = writeln!(self.bytes, "\t.byte {}", bytes.join(","));
        }
        let _ = writeln!(self.bytes, "\t.byte 0");
        self.locs.insert(text.to_string(), id);
        format!(".Lloc{id}")
    }
}

fn validate_main(main_fn: &Function) -> Result<(), Diagnostic> {
    if main_fn.return_type != Some(TypeAnn::Int) {
        return Err(Diagnostic::error(
            format!("not yet compilable: {} not returning int", syntax::ENTRY_FN),
            main_fn.span,
        ));
    }
    Ok(())
}

/// Compiles the checked program to assembly text: every function in
/// every module (like a C translation unit — an unreachable function
/// must still compile). `main_fn` is the entry module's `main`, already
/// verified to exist by the caller.
pub fn compile(
    main_fn: &Function,
    graph: &ModuleGraph,
    res: &Resolutions,
    map: &SourceMap,
) -> Result<String, Diagnostic> {
    validate_main(main_fn)?;

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut asm = String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n");
    let mut strings = Strings::default();
    let mut printers = crate::ir::show::Printers::default();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                asm.push_str(&crate::ir::function(
                    f,
                    mi,
                    res,
                    &mut strings,
                    &mut printers,
                    map,
                )?);
            }
        }
    }
    // The show routines requested by print sites (ADR 0025).
    for ir in printers.build(res, &mut strings) {
        asm.push_str(&crate::ir::emit_function(ir));
    }
    asm.push_str(&runtime());
    asm.push_str(&fmt_f64_runtime());
    asm.push_str(&rodata());
    asm.push_str(&strings.bytes);
    if !strings.descriptors.is_empty() {
        asm.push_str("\t.section .data.rel.ro\n");
        asm.push_str(&strings.descriptors);
    }
    Ok(asm)
}

/// Lowers every checked function in deterministic module/item order and
/// renders pre-register-allocation IR without invoking the assembler.
pub fn dump_ir(
    main_fn: &Function,
    graph: &ModuleGraph,
    res: &Resolutions,
    map: &SourceMap,
) -> Result<String, Diagnostic> {
    validate_main(main_fn)?;
    let mut output = String::new();
    let mut strings = Strings::default();
    let mut printers = crate::ir::show::Printers::default();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                let ir = crate::ir::lower_function(f, mi, res, &mut strings, &mut printers, map)?;
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{ir}");
            }
        }
    }
    for ir in printers.build(res, &mut strings) {
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "{ir}");
    }
    Ok(output)
}

/// The in-assembly runtime, appended to every program. Arrays follow ADR
/// 0014: a handle points at a `{len, cap, data*}` header, elements are
/// inline 8-byte values, buffers come from libc malloc/realloc and are
/// never freed (the arena/leak story of ADR 0009/0015). Both push
/// routines grow by doubling (min 4); `ys_push_n` takes
/// `(hdr, src*, stride_bytes)` and memcpys the element in (ADR 0023).
/// The labels can't collide with user code — every user symbol except
/// the entry `main` carries a `_<module>` suffix.
fn runtime() -> String {
    format!(
        "\
{RT_PUSH}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq 0(%rdi), %rax
\tcmpq 8(%rdi), %rax
\tjb .Lys_push_store
\tmovq 8(%rdi), %rcx
\ttestq %rcx, %rcx
\tjne .Lys_push_double
\tmovq $2, %rcx
.Lys_push_double:
\taddq %rcx, %rcx
\tmovq %rcx, 8(%rdi)
\tpushq %rdi
\tpushq %rsi
\tleaq 0(,%rcx,8), %rsi
\tmovq 16(%rdi), %rdi
\tcall {RT_REALLOC}
\tpopq %rsi
\tpopq %rdi
\tmovq %rax, 16(%rdi)
\tmovq 0(%rdi), %rax
.Lys_push_store:
\tmovq 16(%rdi), %rcx
\tmovq %rsi, (%rcx,%rax,8)
\tincq %rax
\tmovq %rax, 0(%rdi)
\tpopq %rbp
\tret
{RT_PUSH_N}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq 0(%rdi), %rax
\tcmpq 8(%rdi), %rax
\tjb .Lys_push_n_store
\tmovq 8(%rdi), %rcx
\ttestq %rcx, %rcx
\tjne .Lys_push_n_double
\tmovq $2, %rcx
.Lys_push_n_double:
\taddq %rcx, %rcx
\tmovq %rcx, 8(%rdi)
\tpushq %rdi
\tpushq %rsi
\tpushq %rdx
\tpushq %rax
\tmovq %rcx, %rsi
\timulq %rdx, %rsi
\tmovq 16(%rdi), %rdi
\tcall {RT_REALLOC}
\tmovq %rax, %rcx
\tpopq %rax
\tpopq %rdx
\tpopq %rsi
\tpopq %rdi
\tmovq %rcx, 16(%rdi)
.Lys_push_n_store:
\tpushq %rdi
\tpushq %rax
\tmovq 16(%rdi), %rcx
\timulq %rdx, %rax
\taddq %rax, %rcx
\tmovq %rcx, %rdi
\tcall {RT_MEMCPY}
\tpopq %rax
\tpopq %rdi
\tincq %rax
\tmovq %rax, 0(%rdi)
\tpopq %rbp
\tret
{TRAP_OOB}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq %rdx, %r8
\tmovq %rsi, %rcx
\tmovq %rdi, %rdx
\tleaq {FMT_TRAP_OOB}(%rip), %rsi
\tmovl $2, %edi
\txorl %eax, %eax
\tcall {RT_DPRINTF}
\tmovl $1, %edi
\tcall {RT_EXIT}
"
    ) + &one_message_traps()
}

/// The one-message trap stubs (ADR 0022/0028): identical shape, the
/// message register aside — location arrives in %rdi, dprintf reports
/// on stderr, exit 1. The OOB trap stays bespoke (it also carries the
/// index and length).
fn one_message_traps() -> String {
    [
        (TRAP_DIV0, MSG_DIV0),
        (TRAP_OVERFLOW, MSG_OVERFLOW),
        (TRAP_F2I, MSG_F2I),
    ]
    .into_iter()
    .map(|(stub, msg)| {
        format!(
            "\
{stub}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq %rdi, %rcx
\tleaq {msg}(%rip), %rdx
\tleaq {FMT_TRAP}(%rip), %rsi
\tmovl $2, %edi
\txorl %eax, %eax
\tcall {RT_DPRINTF}
\tmovl $1, %edi
\tcall {RT_EXIT}
"
        )
    })
    .collect()
}

/// The float formatter (ADR 0027), one self-contained unit: code, its
/// rodata fragments, and its scratch buffers. The algorithm leans on
/// libc for everything hard — correctly-rounded conversion both ways —
/// and the assembly only orchestrates: probe for the shortest digit
/// string that round-trips, then re-arrange it positionally with at
/// most three `%.*s` fragments. Locale is safe: a C program stays in
/// the "C" locale unless it calls setlocale, which ys programs never
/// do. The full phase story and register roles are in the banner
/// below — they compile into the .s file, so what you debug carries
/// its own explanation.
fn fmt_f64_runtime() -> String {
    format!(
        "\
# ----------------------------------------------------------------
# ys_fmt_f64 — print an f64 exactly as Rust Display does (the
# interpreter's normative text): the shortest digit string that
# parses back to the same bits, positional notation always, no
# trailing newline.
#
#   in: %rdi = the value's BITS (an integer register, so callers
#       movq from XMM registers and spill slots alike)
#
# Phases:
#   S  specials  NaN / inf / -inf / 0 / -0 by bit pattern, no libc
#   1  digits    snprintf(\"%.*e\", p, x) for p = 0,1,2,...: the
#                first output whose strtod parses back bit-equal
#                wins; 17 significant digits (p = 16) always do
#   2  parse     the buffer is \"d.dddde+XX\" or \"-d.dddde-XX\":
#                copy mantissa digits (skipping the point) into
#                .Lys_fmt_digits, read the exponent — no calls here
#   3  render    value = D[0].D[1..n] x 10^E, three shapes:
#                  E >= n-1    digits, then E-(n-1) zeros    (1e21)
#                  0 <= E<n-1  D[..E+1] '.' D[E+1..]         (10.5)
#                  E < 0       \"0.\" then -E-1 zeros, digits (0.03)
#                a zero-length %.*s prints nothing, so the empty
#                zero-run needs no branch of its own
#
# Worked example: -10.5 → p=0 \"-1e+01\" reparses to -10, no;
# p=1 \"-1.0e+01\" → -10, no; p=2 \"-1.05e+01\" → -10.5, yes →
# digits \"105\", n=3, E=1, negative → prints \"-\" \"10\" \".\" \"5\".
#
# Register roles (callee-saved: they live across the libc calls):
#   %rbx  the bits — the round-trip comparison target
#   %r12  p, the snprintf precision being probed
#   %r13  n, how many mantissa digits .Lys_fmt_digits holds
#   %r14  E, the decimal exponent
#   %r15  1 when the value is negative (print '-' first)
# Scratch buffers are static: compiled programs have one thread.
# ----------------------------------------------------------------
{RT_FMT_F64}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx
\tpushq %r12
\tpushq %r13
\tpushq %r14
\tpushq %r15
\tsubq $8, %rsp              # 5 saves + 8 keeps %rsp 16-aligned at calls
\tmovq %rdi, %rbx
# -- S: specials, decided on bits alone ---------------------------
\tmovabsq $0x7fffffffffffffff, %rax
\tandq %rbx, %rax            # |x|: drop the sign bit
\tmovabsq $0x7ff0000000000000, %rcx
\tcmpq %rcx, %rax            # against the infinity pattern
\tjb .Lys_fmt_finite
\tja .Lys_fmt_nan            # above infinity: a NaN payload
\ttestq %rbx, %rbx           # exactly infinity: the sign decides
\tjs .Lys_fmt_neg_inf
\tleaq .Lys_fmt_s_inf(%rip), %rdi
\tjmp .Lys_fmt_special
.Lys_fmt_neg_inf:
\tleaq .Lys_fmt_s_ninf(%rip), %rdi
\tjmp .Lys_fmt_special
.Lys_fmt_nan:
\tleaq .Lys_fmt_s_nan(%rip), %rdi
\tjmp .Lys_fmt_special
.Lys_fmt_zero:
\ttestq %rbx, %rbx
\tjs .Lys_fmt_neg_zero
\tleaq .Lys_fmt_s_zero(%rip), %rdi
\tjmp .Lys_fmt_special
.Lys_fmt_neg_zero:
\tleaq .Lys_fmt_s_nzero(%rip), %rdi
.Lys_fmt_special:
\txorl %eax, %eax
\tcall {RT_PRINTF}           # the text has no '%': it is its own format
\tjmp .Lys_fmt_done
.Lys_fmt_finite:
\ttestq %rax, %rax
\tje .Lys_fmt_zero
# -- 1: find the shortest round-tripping digit string -------------
\txorl %r12d, %r12d          # p = 0 means one significant digit
.Lys_fmt_probe:
\tleaq .Lys_fmt_buf(%rip), %rdi
\tmovl $32, %esi
\tleaq .Lys_fmt_e(%rip), %rdx
\tmovl %r12d, %ecx
\tmovq %rbx, %xmm0
\tmovl $1, %eax              # one vector argument (the double)
\tcall {RT_SNPRINTF}
\tleaq .Lys_fmt_buf(%rip), %rdi
\txorl %esi, %esi            # no end pointer wanted
\tcall {RT_STRTOD}
\tmovq %xmm0, %rax
\tcmpq %rbx, %rax            # bit-exact round trip?
\tje .Lys_fmt_parse
\tincl %r12d
\tcmpl $17, %r12d
\tjb .Lys_fmt_probe          # p = 16 always round-trips; never falls out
# -- 2: parse the buffer (no calls: nothing to keep alive) --------
.Lys_fmt_parse:
\tleaq .Lys_fmt_buf(%rip), %rsi
\txorl %r15d, %r15d
\tcmpb $45, (%rsi)           # '-'
\tjne .Lys_fmt_copy_init
\tmovl $1, %r15d
\tincq %rsi
.Lys_fmt_copy_init:
\tleaq .Lys_fmt_digits(%rip), %rdi
\txorl %r13d, %r13d
.Lys_fmt_copy:
\tmovzbl (%rsi), %eax
\tincq %rsi
\tcmpb $46, %al              # '.': skip it, digits stay contiguous
\tje .Lys_fmt_copy
\tcmpb $101, %al             # 'e': mantissa complete
\tje .Lys_fmt_exp_sign
\tmovb %al, (%rdi,%r13)
\tincq %r13
\tjmp .Lys_fmt_copy
.Lys_fmt_exp_sign:
\txorl %r14d, %r14d
\txorl %ecx, %ecx            # 1 when the exponent is negative
\tcmpb $45, (%rsi)           # '-' (%e always writes a sign)
\tjne .Lys_fmt_exp_digit
\tmovl $1, %ecx
.Lys_fmt_exp_digit:
\tincq %rsi                  # past the sign, then past each digit
\tmovzbl (%rsi), %eax
\ttestb %al, %al             # NUL: exponent complete
\tje .Lys_fmt_exp_done
\timulq $10, %r14, %r14
\tsubl $48, %eax             # '0'
\taddq %rax, %r14
\tjmp .Lys_fmt_exp_digit
.Lys_fmt_exp_done:
\ttestl %ecx, %ecx
\tje .Lys_fmt_render
\tnegq %r14
# -- 3: render positionally, at most three fragments --------------
.Lys_fmt_render:
\ttestl %r15d, %r15d
\tje .Lys_fmt_shape
\tleaq .Lys_fmt_s_minus(%rip), %rdi
\txorl %eax, %eax
\tcall {RT_PRINTF}
.Lys_fmt_shape:
\tleaq -1(%r13), %rax        # n-1: the point's rightmost position
\tcmpq %rax, %r14
\tjl .Lys_fmt_not_whole
# ---- E >= n-1: a whole number — digits, then E-(n-1) zeros ------
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tmovq %r13, %rsi
\tleaq .Lys_fmt_digits(%rip), %rdx
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tmovq %r14, %rsi
\tsubq %r13, %rsi
\tincq %rsi                  # E - (n-1) trailing zeros
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tleaq .Lys_fmt_zeros(%rip), %rdx
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tjmp .Lys_fmt_done
.Lys_fmt_not_whole:
\ttestq %r14, %r14
\tjs .Lys_fmt_below_one
# ---- 0 <= E < n-1: the point sits after digit E+1 ---------------
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tleaq 1(%r14), %rsi         # E+1 digits before the point
\tleaq .Lys_fmt_digits(%rip), %rdx
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tleaq .Lys_fmt_s_dot(%rip), %rdi
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tmovq %r13, %rsi
\tsubq %r14, %rsi
\tdecq %rsi                  # the n-(E+1) digits after the point
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tleaq .Lys_fmt_digits(%rip), %rdx
\taddq %r14, %rdx
\tincq %rdx                  # &digits[E+1]
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tjmp .Lys_fmt_done
.Lys_fmt_below_one:
# ---- E < 0: below one — \"0.\", -E-1 zeros, all the digits -------
\tleaq .Lys_fmt_s_zerodot(%rip), %rdi
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tmovq %r14, %rsi
\tnegq %rsi
\tdecq %rsi                  # -E-1 zeros between the point and digits
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tleaq .Lys_fmt_zeros(%rip), %rdx
\txorl %eax, %eax
\tcall {RT_PRINTF}
\tmovq %r13, %rsi
\tleaq {FMT_STR_RAW}(%rip), %rdi
\tleaq .Lys_fmt_digits(%rip), %rdx
\txorl %eax, %eax
\tcall {RT_PRINTF}
.Lys_fmt_done:
\taddq $8, %rsp
\tpopq %r15
\tpopq %r14
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
# ---- formatter data ----------------------------------------------
\t.section .rodata
.Lys_fmt_e:
\t.string \"%.*e\"
.Lys_fmt_s_nan:
\t.string \"NaN\"
.Lys_fmt_s_inf:
\t.string \"inf\"
.Lys_fmt_s_ninf:
\t.string \"-inf\"
.Lys_fmt_s_zero:
\t.string \"0\"
.Lys_fmt_s_nzero:
\t.string \"-0\"
.Lys_fmt_s_minus:
\t.string \"-\"
.Lys_fmt_s_dot:
\t.string \".\"
.Lys_fmt_s_zerodot:
\t.string \"0.\"
# Longest zero runs: 5e-324 renders \"0.\" + 323 zeros + \"5\", and
# f64::MAX renders 17 digits + 292 zeros. 336 covers both.
.Lys_fmt_zeros:
\t.fill 336, 1, 48           # 336 x '0'
\t.section .bss
.Lys_fmt_buf:
\t.skip 32                   # \"-d.<16 digits>e-308\" + NUL is 26 bytes
.Lys_fmt_digits:
\t.skip 24                   # at most 17 mantissa digits
\t.text
"
    )
}

/// Static formats for `print` (printf needs NUL-terminated formats; ys
/// strings are length-carried, hence `%.*s`).
fn rodata() -> String {
    format!(
        "\
\t.section .rodata
{FMT_INT}:
\t.string \"%ld\\n\"
{FMT_CSTR}:
\t.string \"%s\\n\"
{FMT_STR}:
\t.string \"%.*s\\n\"
{FMT_INT_RAW}:
\t.string \"%ld\"
{FMT_STR_RAW}:
\t.string \"%.*s\"
{TRUE_S}:
\t.string \"true\"
{FALSE_S}:
\t.string \"false\"
{NULL_S}:
\t.string \"null\"
{FMT_TRAP}:
\t.string \"error: %s\\n --> %s\\n\"
{FMT_TRAP_OOB}:
\t.string \"error: index %ld out of bounds (length %ld)\\n --> %s\\n\"
{MSG_DIV0}:
\t.string \"division by zero\"
{MSG_OVERFLOW}:
\t.string \"division overflow\"
{MSG_F2I}:
\t.string \"invalid float to int conversion\"
"
    )
}
