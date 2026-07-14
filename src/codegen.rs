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

/// The in-assembly array-push runtime routine.
pub(crate) const RT_PUSH: &str = "ys_push";
pub(crate) const RT_PRINTF: &str = "printf@PLT";
pub(crate) const RT_MALLOC: &str = "malloc@PLT";
pub(crate) const RT_REALLOC: &str = "realloc@PLT";
pub(crate) const RT_MEMCPY: &str = "memcpy@PLT";
pub(crate) const RT_MEMCMP: &str = "memcmp@PLT";
pub(crate) const RT_FMOD: &str = "fmod@PLT";
pub(crate) const RT_DPRINTF: &str = "dprintf@PLT";
// abort left the inventory with ADR 0022: traps report and exit 1.
pub(crate) const RT_EXIT: &str = "exit@PLT";

/// Trap stubs (ADR 0022): print a runtime diagnostic and exit 1.
pub(crate) const TRAP_DIV0: &str = "ys_trap_div0";
pub(crate) const TRAP_OVERFLOW: &str = "ys_trap_overflow";
pub(crate) const TRAP_OOB: &str = "ys_trap_oob";

/// printf formats and fixed strings for `print`.
pub(crate) const FMT_INT: &str = ".Lfmt_int";
pub(crate) const FMT_CSTR: &str = ".Lfmt_cstr";
pub(crate) const FMT_STR: &str = ".Lfmt_str";
pub(crate) const TRUE_S: &str = ".Ltrue_s";
pub(crate) const FALSE_S: &str = ".Lfalse_s";
pub(crate) const NULL_S: &str = ".Lnull_s";
pub(crate) const FMT_TRAP: &str = ".Lfmt_trap";
pub(crate) const FMT_TRAP_OOB: &str = ".Lfmt_trap_oob";
pub(crate) const MSG_DIV0: &str = ".Lmsg_div0";
pub(crate) const MSG_OVERFLOW: &str = ".Lmsg_overflow";

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

    /// Interns a NUL-terminated location string for trap diagnostics
    /// (ADR 0022); returns its label. Deduplicated per program.
    pub(crate) fn intern_loc(&mut self, text: &str) -> String {
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
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                asm.push_str(&crate::ir::function(f, mi, res, &mut strings, map)?);
            }
        }
    }
    asm.push_str(&runtime());
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
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                let ir = crate::ir::lower_function(f, mi, res, &mut strings, map)?;
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{ir}");
            }
        }
    }
    Ok(output)
}

/// The in-assembly runtime, appended to every program. Arrays follow ADR
/// 0014: a handle points at a `{len, cap, data*}` header, elements are
/// inline 8-byte values, buffers come from libc malloc/realloc and are
/// never freed (the arena/leak story of ADR 0009/0015). `ys_push` grows
/// by doubling (min 4). The label can't collide with user code — every
/// user symbol except the entry `main` carries a `_<module>` suffix.
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
{TRAP_DIV0}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq %rdi, %rcx
\tleaq {MSG_DIV0}(%rip), %rdx
\tleaq {FMT_TRAP}(%rip), %rsi
\tmovl $2, %edi
\txorl %eax, %eax
\tcall {RT_DPRINTF}
\tmovl $1, %edi
\tcall {RT_EXIT}
{TRAP_OVERFLOW}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq %rdi, %rcx
\tleaq {MSG_OVERFLOW}(%rip), %rdx
\tleaq {FMT_TRAP}(%rip), %rsi
\tmovl $2, %edi
\txorl %eax, %eax
\tcall {RT_DPRINTF}
\tmovl $1, %edi
\tcall {RT_EXIT}
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
"
    )
}
