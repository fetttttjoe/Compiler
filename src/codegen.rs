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
/// The shared text builder (ADR 0029): `ys_sb_append` grows the static
/// byte buffer and copies bytes in; `ys_sb_int` renders one i64 into it.
pub(crate) const RT_SB_APPEND: &str = "ys_sb_append";
pub(crate) const RT_SB_INT: &str = "ys_sb_int";
/// The world interface (ADR 0031): argv materialization, file handles
/// (heap boxes `{FILE*, closed}`), and line input.
pub(crate) const RT_ARGS: &str = "ys_args";
pub(crate) const RT_OPEN: &str = "ys_open";
pub(crate) const RT_READ: &str = "ys_read";
pub(crate) const RT_READLINE: &str = "ys_readline";
pub(crate) const RT_WRITE: &str = "ys_write";
pub(crate) const RT_CLOSE: &str = "ys_close";
/// The builder's `{len, cap, ptr}` header: lowered code stores len = 0
/// to reset and reads `{len, ptr}` to consume the bytes.
pub(crate) const SB_HDR: &str = ".Lys_sb";
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
// libc pieces of the world interface (ADR 0031).
pub(crate) const RT_FOPEN: &str = "fopen@PLT";
pub(crate) const RT_FCLOSE: &str = "fclose@PLT";
pub(crate) const RT_FREAD: &str = "fread@PLT";
pub(crate) const RT_FWRITE: &str = "fwrite@PLT";
pub(crate) const RT_FFLUSH: &str = "fflush@PLT";
pub(crate) const RT_GETLINE: &str = "getline@PLT";
pub(crate) const RT_STRLEN: &str = "strlen@PLT";
pub(crate) const RT_STRTOD: &str = "strtod@PLT";
// abort left the inventory with ADR 0022: traps report and exit 1.
pub(crate) const RT_EXIT: &str = "exit@PLT";
/// `main(): int!` escaping with an error (ADR 0034): prints the
/// builder's bytes as `error: …` on stderr and exits 1. CALL-entered
/// like every stub — never `jmp` (stack alignment).
pub(crate) const RT_ERR_EXIT: &str = "ys_err_exit";
/// The `main(): int!` implementation label: the dot keeps it out of
/// user-identifier space (the show-routine convention).
pub(crate) const ENTRY_IMPL: &str = "ys.main";

/// Trap stubs (ADR 0022): print a runtime diagnostic and exit 1.
pub(crate) const TRAP_DIV0: &str = "ys_trap_div0";
pub(crate) const TRAP_OVERFLOW: &str = "ys_trap_overflow";
pub(crate) const TRAP_OOB: &str = "ys_trap_oob";
pub(crate) const TRAP_F2I: &str = "ys_trap_f2i";
pub(crate) const TRAP_CLOSED: &str = "ys_trap_closed";
pub(crate) const TRAP_READSIZE: &str = "ys_trap_readsize";

/// printf formats and fixed strings for `print`. `FMT_INT_RAW` carries
/// no newline — it is `ys_sb_int`'s snprintf format (ADR 0029).
pub(crate) const FMT_INT: &str = ".Lfmt_int";
pub(crate) const FMT_INT_RAW: &str = ".Lfmt_int_raw";
pub(crate) const FMT_CSTR: &str = ".Lfmt_cstr";
pub(crate) const FMT_STR: &str = ".Lfmt_str";
pub(crate) const FMT_ERR_EXIT: &str = ".Lfmt_err_exit";
pub(crate) const TRUE_S: &str = ".Ltrue_s";
pub(crate) const FALSE_S: &str = ".Lfalse_s";
pub(crate) const NULL_S: &str = ".Lnull_s";
pub(crate) const FMT_TRAP: &str = ".Lfmt_trap";
pub(crate) const FMT_TRAP_OOB: &str = ".Lfmt_trap_oob";
pub(crate) const MSG_DIV0: &str = ".Lmsg_div0";
pub(crate) const MSG_OVERFLOW: &str = ".Lmsg_overflow";
pub(crate) const MSG_F2I: &str = ".Lmsg_f2i";
pub(crate) const MSG_CLOSED: &str = ".Lmsg_closed";
pub(crate) const MSG_READSIZE: &str = ".Lmsg_readsize";

/// The assembly symbol for a function: the entry `main` keeps its name
/// (the C runtime calls it); everything else is suffixed with its module
/// index, which decodes uniquely (the suffix after the last underscore).
pub(crate) fn label_of(module: usize, name: &str) -> String {
    if module == 0 && name == syntax::ENTRY_FN {
        name.to_string()
    } else {
        format!("{}_{module}", sanitize(name))
    }
}

/// Assembler-safe instance names (ADR 0035): the canonical mangle's
/// specials map injectively onto `$`-codes (`$` never appears in user
/// identifiers, so distinct canonicals stay distinct labels). Source
/// names pass through untouched.
pub(crate) fn sanitize(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() + 8);
    for c in name.chars() {
        match c {
            '<' => out.push_str("$l"),
            '>' => out.push_str("$g"),
            ',' => out.push_str("$c"),
            '#' => out.push_str("$h"),
            '?' => out.push_str("$q"),
            '!' => out.push_str("$e"),
            '[' => out.push_str("$b"),
            ']' => out.push_str("$d"),
            ' ' => {}
            other => out.push(other),
        }
    }
    out
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

/// `main` returns `int` (the exit code) or `int!` (ADR 0034 decision
/// 8: an escaping error prints and exits 1). Returns whether the entry
/// needs the tag-testing wrapper.
fn validate_main(main_fn: &Function) -> Result<bool, Diagnostic> {
    match &main_fn.return_type {
        Some(TypeAnn::Int) => Ok(false),
        Some(TypeAnn::ErrUnion(inner)) if **inner == TypeAnn::Int => Ok(true),
        _ => Err(Diagnostic::error(
            format!(
                "not yet compilable: {} not returning int or int!",
                syntax::ENTRY_FN
            ),
            main_fn.span,
        )),
    }
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
    let entry_errs = validate_main(main_fn)?;

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut asm = String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n");
    let mut strings = Strings::default();
    let mut printers = crate::ir::show::Printers::default();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            // Generic templates compile per instance, below (ADR 0035).
            if let Item::Function(f) = item
                && f.type_params.is_empty()
            {
                // The int!-returning entry moves aside (sret convention);
                // the C `main` below adapts (ADR 0034 decision 8).
                if entry_errs && mi == 0 && f.name == syntax::ENTRY_FN {
                    asm.push_str(&crate::ir::function_as(
                        f,
                        mi,
                        res,
                        &mut strings,
                        &mut printers,
                        map,
                        ENTRY_IMPL,
                    )?);
                    continue;
                }
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
    // Monomorphized instances, in sorted order — assembly output stays
    // deterministic (ADR 0035).
    let mut instance_keys: Vec<&(usize, String)> = res.instances.keys().collect();
    instance_keys.sort();
    for key in instance_keys {
        let f = &res.instances[key];
        asm.push_str(&crate::ir::function(
            f,
            key.0,
            res,
            &mut strings,
            &mut printers,
            map,
        )?);
    }
    if entry_errs {
        // The wrapper: forward argc/argv behind the sret pointer, then
        // exit with the payload — or render the code via its show
        // routine (builder reset first, consumer discipline) and take
        // the trap-shaped exit.
        let show = printers.request(&crate::types::Type::ErrCode, res);
        let impl_label = label_of(0, ENTRY_IMPL);
        let show_label = label_of(0, &show);
        let _ = write!(
            asm,
            "\t.globl main\nmain:\n\
             \tpushq %rbp\n\tmovq %rsp, %rbp\n\tsubq $16, %rsp\n\
             \tmovq %rsi, %rdx\n\tmovq %rdi, %rsi\n\tleaq -16(%rbp), %rdi\n\
             \tcall {impl_label}\n\
             \tmovq -16(%rbp), %rdi\n\tcmpq $2, %rdi\n\tjl .Lys_main_ok\n\
             \tmovq $0, {SB_HDR}(%rip)\n\tmovq $8, %rsi\n\tcall {show_label}\n\
             \tcall {RT_ERR_EXIT}\n\
             .Lys_main_ok:\n\tmovq -8(%rbp), %rax\n\tleave\n\tret\n"
        );
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
            if let Item::Function(f) = item
                && f.type_params.is_empty()
            {
                let ir = crate::ir::lower_function(f, mi, res, &mut strings, &mut printers, map)?;
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{ir}");
            }
        }
    }
    let mut instance_keys: Vec<&(usize, String)> = res.instances.keys().collect();
    instance_keys.sort();
    for key in instance_keys {
        let f = &res.instances[key];
        let ir = crate::ir::lower_function(f, key.0, res, &mut strings, &mut printers, map)?;
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "{ir}");
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
        (TRAP_CLOSED, MSG_CLOSED),
        (TRAP_READSIZE, MSG_READSIZE),
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
    .collect::<String>()
        + &sb_runtime()
        + &io_runtime()
}

/// The world interface (ADR 0031). A file handle is a heap box
/// `{FILE*, closed}` — the closed flag is what turns use-after-close
/// into the diagnosed trap the interpreter also reports, instead of
/// libc UB. Environmental failure returns null/false; `read`/`readLine`
/// fill a caller-provided `{tag, ptr, len}` value optional (ADR 0021).
/// Writes fflush every time: the oracle's writes are unbuffered, and
/// write-then-reopen-then-read must agree across engines.
fn io_runtime() -> String {
    format!(
        "\
{RT_ARGS}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx
\tpushq %r12
\tpushq %r13
\tpushq %r14
\tpushq %r15
\tsubq $8, %rsp
\tmovq %rdi, %r12            # argc (>= 1 under any real crt)
\tmovq %rsi, %r13            # argv
\tcmpq $1, %r12
\tjge .Lys_args_hdr
\tmovq $1, %r12
.Lys_args_hdr:
\tmovq $24, %rdi             # array header {{len, cap, data*}}
\tcall {RT_MALLOC}
\tmovq %rax, %r14
\tleaq -1(%r12), %rax        # n = argc - 1: argv[0] is not an arg
\tmovq %rax, 0(%r14)
\tmovq %rax, 8(%r14)
\tshlq $4, %rax              # string descriptors are 16 bytes
\tmovq %rax, %rdi
\tcall {RT_MALLOC}
\tmovq %rax, 16(%r14)
\tmovq %rax, %r15
.Lys_args_loop:
\tcmpq $1, %r12
\tjle .Lys_args_done
\tmovq 8(%r13), %rbx         # the next argument's bytes
\taddq $8, %r13
\tdecq %r12
\tmovq %rbx, %rdi
\tcall {RT_STRLEN}
\tmovq %rbx, 0(%r15)         # descriptor {{ptr, len}}
\tmovq %rax, 8(%r15)
\taddq $16, %r15
\tjmp .Lys_args_loop
.Lys_args_done:
\tmovq %r14, %rax
\taddq $8, %rsp
\tpopq %r15
\tpopq %r14
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
{RT_OPEN}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx                 # path descriptor
\tpushq %r12                 # mode cstr
\tpushq %r13                 # NUL-terminated path copy
\tpushq %r14                 # FILE*
\tmovq %rdi, %rbx
# -- the mode set is pinned to r/w/a (ADR 0031): both engines validate
#    it themselves, so fopen's extended modes can't diverge
\tcmpq $1, 8(%rsi)
\tjne .Lys_open_fail
\tmovq 0(%rsi), %rax
\tmovzbl (%rax), %eax
\tleaq .Lys_mode_r(%rip), %r12
\tcmpb $114, %al             # 'r'
\tje .Lys_open_path
\tleaq .Lys_mode_w(%rip), %r12
\tcmpb $119, %al             # 'w'
\tje .Lys_open_path
\tleaq .Lys_mode_a(%rip), %r12
\tcmpb $97, %al              # 'a'
\tjne .Lys_open_fail
.Lys_open_path:
\tmovq 8(%rbx), %rdi         # fopen needs NUL termination; a path
\tincq %rdi                  # with an embedded NUL can't name a file
\tcall {RT_MALLOC}
\tmovq %rax, %r13
\tmovq %rax, %rdi
\tmovq 0(%rbx), %rsi
\tmovq 8(%rbx), %rdx
\tcall {RT_MEMCPY}
\tmovq 8(%rbx), %rax
\tmovb $0, 0(%r13,%rax)
\tmovq %r13, %rdi
\tcall {RT_STRLEN}
\tcmpq 8(%rbx), %rax         # embedded NUL: strlen comes up short
\tjne .Lys_open_fail
\tmovq %r13, %rdi
\tmovq %r12, %rsi
\tcall {RT_FOPEN}
\ttestq %rax, %rax
\tje .Lys_open_fail
\tmovq %rax, %r14
\tmovq $16, %rdi             # the handle box {{FILE*, closed}}
\tcall {RT_MALLOC}
\tmovq %r14, 0(%rax)
\tmovq $0, 8(%rax)
\tjmp .Lys_open_ret
.Lys_open_fail:
\txorl %eax, %eax
.Lys_open_ret:
\tpopq %r14
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
{RT_READ}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx                 # box
\tpushq %r12                 # max
\tpushq %r13                 # dst optional {{tag, ptr, len}}
\tpushq %r14                 # buffer
\tmovq %rdi, %rbx
\tmovq %rsi, %r12
\tmovq %rdx, %r13
\tcmpq $0, 8(%rdi)
\tje .Lys_read_open
\tmovq %rcx, %rdi
\tcall {TRAP_CLOSED}
.Lys_read_open:
\ttestq %r12, %r12
\tjg .Lys_read_sized
\tmovq %rcx, %rdi
\tcall {TRAP_READSIZE}
.Lys_read_sized:
\tmovq %r12, %rdi
\tcall {RT_MALLOC}
\tmovq %rax, %r14
\tmovq %rax, %rdi            # fread(buf, 1, max, f): loops short
\tmovl $1, %esi              # reads internally — the oracle mirrors
\tmovq %r12, %rdx
\tmovq 0(%rbx), %rcx
\tcall {RT_FREAD}
\ttestq %rax, %rax
\tjne .Lys_read_some
\tmovq $0, 0(%r13)           # EOF/error: null (zeroed payload)
\tmovq $0, 8(%r13)
\tmovq $0, 16(%r13)
\tjmp .Lys_read_ret
.Lys_read_some:
\tmovq $1, 0(%r13)
\tmovq %r14, 8(%r13)
\tmovq %rax, 16(%r13)
.Lys_read_ret:
\tpopq %r14
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
{RT_READLINE}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx                 # FILE*
\tpushq %r12                 # dst optional {{tag, ptr, len}}
\tpushq %r13                 # line length
\tpushq %r14                 # copied-out bytes
\tmovq %rsi, %r12
\ttestq %rdi, %rdi           # box 0 is the stdin form (arity 0):
\tje .Lys_rl_stdin           # only the lowering emits it
\tcmpq $0, 8(%rdi)
\tje .Lys_rl_file
\tmovq %rdx, %rdi
\tcall {TRAP_CLOSED}
.Lys_rl_file:
\tmovq 0(%rdi), %rbx
\tjmp .Lys_rl_go
.Lys_rl_stdin:
\tmovq stdin@GOTPCREL(%rip), %rax
\tmovq (%rax), %rbx
.Lys_rl_go:
\tleaq .Lys_rl_buf(%rip), %rdi     # getline reuses one static buffer
\tleaq .Lys_rl_cap(%rip), %rsi
\tmovq %rbx, %rdx
\tcall {RT_GETLINE}
\tcmpq $0, %rax
\tjg .Lys_rl_some
\tmovq $0, 0(%r12)           # EOF/error: null (zeroed payload)
\tmovq $0, 8(%r12)
\tmovq $0, 16(%r12)
\tjmp .Lys_rl_ret
.Lys_rl_some:
\tmovq %rax, %r13
\tleaq .Lys_rl_buf(%rip), %rax
\tmovq (%rax), %rax
\tcmpb $10, -1(%rax,%r13)    # strip one trailing newline
\tjne .Lys_rl_copy
\tdecq %r13                  # an empty line is a 0-length string
.Lys_rl_copy:
\tmovq %r13, %rdi
\tcall {RT_MALLOC}
\tmovq %rax, %r14
\tmovq %rax, %rdi
\tleaq .Lys_rl_buf(%rip), %rax
\tmovq (%rax), %rsi
\tmovq %r13, %rdx
\tcall {RT_MEMCPY}
\tmovq $1, 0(%r12)
\tmovq %r14, 8(%r12)
\tmovq %r13, 16(%r12)
.Lys_rl_ret:
\tpopq %r14
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
{RT_WRITE}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx                 # box
\tpushq %r12                 # string descriptor
\tpushq %r13                 # length
\tsubq $8, %rsp
\tmovq %rdi, %rbx
\tmovq %rsi, %r12
\tcmpq $0, 8(%rdi)
\tje .Lys_write_open
\tmovq %rdx, %rdi
\tcall {TRAP_CLOSED}
.Lys_write_open:
\tmovq 0(%r12), %rdi         # fwrite(ptr, 1, len, f)
\tmovl $1, %esi
\tmovq 8(%r12), %rdx
\tmovq %rdx, %r13
\tmovq 0(%rbx), %rcx
\tcall {RT_FWRITE}
\tcmpq %r13, %rax
\tjne .Lys_write_fail
\tmovq 0(%rbx), %rdi         # fflush per write: oracle writes are
\tcall {RT_FFLUSH}           # unbuffered, reopen-and-read must agree
\ttestl %eax, %eax
\tjne .Lys_write_fail
\tmovl $1, %eax
\tjmp .Lys_write_ret
.Lys_write_fail:
\txorl %eax, %eax
.Lys_write_ret:
\taddq $8, %rsp
\tpopq %r13
\tpopq %r12
\tpopq %rbx
\tpopq %rbp
\tret
{RT_CLOSE}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tpushq %rbx
\tsubq $8, %rsp
\tmovq %rdi, %rbx
\tcmpq $0, 8(%rdi)
\tje .Lys_close_open
\tmovq %rsi, %rdi
\tcall {TRAP_CLOSED}
.Lys_close_open:
\tmovq $1, 8(%rbx)           # closed before fclose: the box outlives it
\tmovq 0(%rbx), %rdi
\tcall {RT_FCLOSE}
\ttestl %eax, %eax
\tsete %al
\tmovzbl %al, %eax
\taddq $8, %rsp
\tpopq %rbx
\tpopq %rbp
\tret
\t.section .rodata
.Lys_mode_r:
\t.string \"r\"
.Lys_mode_w:
\t.string \"w\"
.Lys_mode_a:
\t.string \"a\"
\t.section .bss
.Lys_rl_buf:
\t.skip 8                    # getline's buffer pointer (grows, reused)
.Lys_rl_cap:
\t.skip 8
\t.text
"
    )
}

/// The shared text builder (ADR 0029): one static `{len, cap, ptr}`
/// byte buffer that the show routines, `ys_fmt_f64`, and `ys_sb_int`
/// append into; `print` and `string()` reset it, run producers, then
/// consume the bytes. Static is sound — compiled programs are
/// single-threaded (the ADR 0027 scratch precedent) — and growth
/// mirrors the array runtime: doubling, realloc from NULL, never freed
/// (ADR 0015).
fn sb_runtime() -> String {
    format!(
        "\
{RT_ERR_EXIT}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tleaq {SB_HDR}(%rip), %rax
\tmovq 0(%rax), %rdx
\tmovq 16(%rax), %rcx
\tleaq {FMT_ERR_EXIT}(%rip), %rsi
\tmovl $2, %edi
\txorl %eax, %eax
\tcall {RT_DPRINTF}
\tmovl $1, %edi
\tcall {RT_EXIT}
{RT_SB_APPEND}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tleaq {SB_HDR}(%rip), %rax
\tmovq 0(%rax), %rcx
\tleaq (%rcx,%rsi), %rdx     # need = len + n
\tcmpq 8(%rax), %rdx
\tjbe .Lys_sb_copy
\tmovq 8(%rax), %rcx
\ttestq %rcx, %rcx
\tjne .Lys_sb_grow
\tmovq $16, %rcx             # first growth lands on 32
.Lys_sb_grow:
\taddq %rcx, %rcx
\tcmpq %rdx, %rcx
\tjb .Lys_sb_grow
\tmovq %rcx, 8(%rax)
\tpushq %rdi
\tpushq %rsi
\tmovq %rcx, %rsi            # realloc(ptr, newcap); NULL ptr mallocs
\tmovq 16(%rax), %rdi
\tcall {RT_REALLOC}
\tpopq %rsi
\tpopq %rdi
\tmovq %rax, %rcx
\tleaq {SB_HDR}(%rip), %rax
\tmovq %rcx, 16(%rax)
.Lys_sb_copy:
\tmovq %rsi, %rcx            # rep movsb wants (dst rdi, src rsi, n rcx)
\tmovq %rdi, %rsi
\tmovq 16(%rax), %rdi
\taddq 0(%rax), %rdi
\taddq %rcx, 0(%rax)         # len += n; n = 0 copies nothing
\trep movsb
\tpopq %rbp
\tret
{RT_SB_INT}:
\tpushq %rbp
\tmovq %rsp, %rbp
\tmovq %rdi, %rcx            # snprintf(scratch, 24, \"%ld\", value)
\tleaq {FMT_INT_RAW}(%rip), %rdx
\tmovl $24, %esi
\tleaq .Lys_sb_scratch(%rip), %rdi
\txorl %eax, %eax
\tcall {RT_SNPRINTF}
\tmovl %eax, %esi            # the count — an i64 is at most 20 chars
\tleaq .Lys_sb_scratch(%rip), %rdi
\tpopq %rbp
\tjmp {RT_SB_APPEND}
\t.section .bss
{SB_HDR}:
\t.skip 24                   # {{len, cap, ptr}} — zeroed at load
.Lys_sb_scratch:
\t.skip 24
\t.text
"
    )
}

/// The float formatter (ADR 0027), one self-contained unit: code, its
/// rodata fragments, and its scratch buffers. The algorithm leans on
/// libc for everything hard — correctly-rounded conversion both ways —
/// and the assembly only orchestrates: probe for the shortest digit
/// string that round-trips, then re-arrange it positionally with at
/// most three appended fragments. Locale is safe: a C program stays in
/// the "C" locale unless it calls setlocale, which ys programs never
/// do. The full phase story and register roles are in the banner
/// below — they compile into the .s file, so what you debug carries
/// its own explanation.
fn fmt_f64_runtime() -> String {
    format!(
        "\
# ----------------------------------------------------------------
# ys_fmt_f64 — append an f64's text to the builder, exactly as Rust
# Display writes it (the interpreter's normative text): the shortest
# digit string that parses back to the same bits, positional notation
# always. Consumers reset the builder before and use its bytes after.
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
#                a zero-length append is a no-op, so the empty
#                zero-run needs no branch of its own
#
# Worked example: -10.5 → p=0 \"-1e+01\" reparses to -10, no;
# p=1 \"-1.0e+01\" → -10, no; p=2 \"-1.05e+01\" → -10.5, yes →
# digits \"105\", n=3, E=1, negative → appends \"-\" \"10\" \".\" \"5\".
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
\tmovl $3, %esi
\tjmp .Lys_fmt_special
.Lys_fmt_neg_inf:
\tleaq .Lys_fmt_s_ninf(%rip), %rdi
\tmovl $4, %esi
\tjmp .Lys_fmt_special
.Lys_fmt_nan:
\tleaq .Lys_fmt_s_nan(%rip), %rdi
\tmovl $3, %esi
\tjmp .Lys_fmt_special
.Lys_fmt_zero:
\ttestq %rbx, %rbx
\tjs .Lys_fmt_neg_zero
\tleaq .Lys_fmt_s_zero(%rip), %rdi
\tmovl $1, %esi
\tjmp .Lys_fmt_special
.Lys_fmt_neg_zero:
\tleaq .Lys_fmt_s_nzero(%rip), %rdi
\tmovl $2, %esi
.Lys_fmt_special:
\tcall {RT_SB_APPEND}
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
\tmovl $1, %esi
\tcall {RT_SB_APPEND}
.Lys_fmt_shape:
\tleaq -1(%r13), %rax        # n-1: the point's rightmost position
\tcmpq %rax, %r14
\tjl .Lys_fmt_not_whole
# ---- E >= n-1: a whole number — digits, then E-(n-1) zeros ------
\tleaq .Lys_fmt_digits(%rip), %rdi
\tmovq %r13, %rsi
\tcall {RT_SB_APPEND}
\tmovq %r14, %rsi
\tsubq %r13, %rsi
\tincq %rsi                  # E - (n-1) trailing zeros, possibly none
\tleaq .Lys_fmt_zeros(%rip), %rdi
\tcall {RT_SB_APPEND}
\tjmp .Lys_fmt_done
.Lys_fmt_not_whole:
\ttestq %r14, %r14
\tjs .Lys_fmt_below_one
# ---- 0 <= E < n-1: the point sits after digit E+1 ---------------
\tleaq .Lys_fmt_digits(%rip), %rdi
\tleaq 1(%r14), %rsi         # E+1 digits before the point
\tcall {RT_SB_APPEND}
\tleaq .Lys_fmt_s_dot(%rip), %rdi
\tmovl $1, %esi
\tcall {RT_SB_APPEND}
\tleaq .Lys_fmt_digits(%rip), %rdi
\taddq %r14, %rdi
\tincq %rdi                  # &digits[E+1]
\tmovq %r13, %rsi
\tsubq %r14, %rsi
\tdecq %rsi                  # the n-(E+1) digits after the point
\tcall {RT_SB_APPEND}
\tjmp .Lys_fmt_done
.Lys_fmt_below_one:
# ---- E < 0: below one — \"0.\", -E-1 zeros, all the digits -------
\tleaq .Lys_fmt_s_zerodot(%rip), %rdi
\tmovl $2, %esi
\tcall {RT_SB_APPEND}
\tmovq %r14, %rsi
\tnegq %rsi
\tdecq %rsi                  # -E-1 zeros between the point and digits
\tleaq .Lys_fmt_zeros(%rip), %rdi
\tcall {RT_SB_APPEND}
\tleaq .Lys_fmt_digits(%rip), %rdi
\tmovq %r13, %rsi
\tcall {RT_SB_APPEND}
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
{TRUE_S}:
\t.string \"true\"
{FALSE_S}:
\t.string \"false\"
{NULL_S}:
\t.string \"null\"
{FMT_TRAP}:
\t.string \"error: %s\\n --> %s\\n\"
{FMT_ERR_EXIT}:
\t.string \"error: %.*s\\n\"
{FMT_TRAP_OOB}:
\t.string \"error: index %ld out of bounds (length %ld)\\n --> %s\\n\"
{MSG_DIV0}:
\t.string \"division by zero\"
{MSG_OVERFLOW}:
\t.string \"division overflow\"
{MSG_F2I}:
\t.string \"invalid float to int conversion\"
{MSG_CLOSED}:
\t.string \"operation on closed file\"
{MSG_READSIZE}:
\t.string \"read size must be positive\"
"
    )
}
