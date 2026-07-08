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
use std::collections::HashMap;
use std::fmt::Write;

/// The assembly symbol for a function: the entry `main` keeps its name
/// (the C runtime calls it); everything else is suffixed with its module
/// index, which decodes uniquely (the suffix after the last underscore).
pub(crate) fn label_of(module: usize, name: &str) -> String {
    if module == 0 && name == "main" {
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
}

impl Strings {
    pub(crate) fn intern(&mut self, text: &str) -> usize {
        if let Some(&id) = self.ids.get(text) {
            return id;
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
        id
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
) -> Result<String, Diagnostic> {
    if main_fn.return_type != Some(TypeAnn::Int) {
        return Err(Diagnostic::error(
            "not yet compilable: main not returning int".to_string(),
            main_fn.span,
        ));
    }

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut asm = String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n");
    let mut strings = Strings::default();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                asm.push_str(&crate::ir::function(f, mi, res, &mut strings)?);
            }
        }
    }
    asm.push_str(RUNTIME);
    asm.push_str(RODATA);
    asm.push_str(&strings.bytes);
    if !strings.descriptors.is_empty() {
        asm.push_str("\t.section .data.rel.ro\n");
        asm.push_str(&strings.descriptors);
    }
    Ok(asm)
}

/// The in-assembly runtime, appended to every program. Arrays follow ADR
/// 0014: a handle points at a `{len, cap, data*}` header, elements are
/// inline 8-byte values, buffers come from libc malloc/realloc and are
/// never freed (the arena/leak story of ADR 0009/0015). `ys_push` grows
/// by doubling (min 4). The label can't collide with user code — every
/// user symbol except the entry `main` carries a `_<module>` suffix.
const RUNTIME: &str = "\
ys_push:
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
\tcall realloc@PLT
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
";

/// Static formats for `print` (printf needs NUL-terminated formats; ys
/// strings are length-carried, hence `%.*s`).
const RODATA: &str = "\
\t.section .rodata
.Lfmt_int:
\t.string \"%ld\\n\"
.Lfmt_cstr:
\t.string \"%s\\n\"
.Lfmt_str:
\t.string \"%.*s\\n\"
.Ltrue_s:
\t.string \"true\"
.Lfalse_s:
\t.string \"false\"
";
