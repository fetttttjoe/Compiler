//! x86-64 backend (ADR 0009): compiles the program to AT&T assembly for
//! the system `cc` to assemble and link. Covers int/bool arithmetic,
//! comparisons, short-circuit logic, locals, `if`/`while`/`for`, direct
//! calls, arrays, refstructs with reference optionals, and value structs.
//! Everything else is a clean "not yet compilable" diagnostic; breadth
//! arrives slice by slice, each diffed against the interpreter
//! (see tests/diff.rs).
//!
//! Scheme: recursive emission into %rax, machine stack for the pending
//! left operand. Every expression value is one word — scalars and
//! handles by value; value structs and strings by *pointer* to storage,
//! copied wherever the oracle copies: `let`, assignment, return, each
//! call argument at evaluation time, and equality's left operand (a
//! later evaluation may mutate the storage through a refstruct alias).
//! ponytail: no IR and no register allocation until a real optimization
//! needs them, per ADR 0009.
//!
//! Compiled behavior on the idiv traps — division by zero and
//! i64::MIN / -1 — is deferred (the binary takes a SIGFPE): the
//! interpreter diagnoses both, and the differential harness only diffs
//! programs the interpreter runs cleanly.
//!
//! Standing obligation for later slices: the first data symbol (string
//! literal, global) must use RIP-relative addressing — the system cc
//! links PIE by default.

use crate::ast::{BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;
use crate::types::Type;
use std::collections::HashMap;
use std::fmt::Write;

/// System V integer argument registers, in order.
const ARG_REGS: [&str; 6] = ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"];

/// Recursion bound for layout walks — a recursive value struct has
/// infinite size; its values can't exist, so hitting this is diagnostic.
const FUEL: usize = 64;

/// Compiles the checked program to assembly text: every function in every
/// module (like a C translation unit — an unreachable function must still
/// compile), calls resolved through the same alias maps the interpreter
/// uses. `main_fn` is the entry module's `main`, already verified to
/// exist by the caller.
pub fn compile(
    main_fn: &Function,
    graph: &ModuleGraph,
    res: &Resolutions,
) -> Result<String, Diagnostic> {
    if main_fn.return_type != Some(TypeAnn::Int) {
        return Err(unsupported("main not returning int", main_fn.span));
    }

    // The GNU-stack note marks the stack non-executable; without it the
    // linker warns and grants an executable stack.
    let mut e = Emitter {
        asm: String::from("\t.section .note.GNU-stack,\"\",@progbits\n\t.text\n"),
        scopes: Vec::new(),
        next_slot: 0,
        min_slot: 0,
        labels: 0,
        depth: 0,
        module: 0,
        ret_words: 1,
        sret_slot: None,
        res,
        data: String::new(),
        relro: String::new(),
        str_ids: HashMap::new(),
    };

    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                e.function(f, mi)?;
            }
        }
    }
    e.asm.push_str(RUNTIME);
    // Read-only data: printf formats, bool spellings, string bytes — all
    // RIP-relative (the system cc links PIE by default). Descriptors hold
    // an absolute address needing a load-time relocation, so they live in
    // .data.rel.ro, not .rodata (TEXTREL would break `-z text` linking).
    e.asm.push_str(RODATA);
    e.asm.push_str(&e.data);
    if !e.relro.is_empty() {
        e.asm.push_str("\t.section .data.rel.ro\n");
        e.asm.push_str(&e.relro);
    }
    Ok(e.asm)
}

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

/// The assembly symbol for a function: the entry `main` keeps its name
/// (the C runtime calls it); everything else is suffixed with its module
/// index, which decodes uniquely (the suffix after the last underscore).
fn label_of(module: usize, name: &str) -> String {
    if module == 0 && name == "main" {
        name.to_string()
    } else {
        format!("{name}_{module}")
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

/// The backend's view of a value: one word (scalars, handles), a str
/// (two-word fat pointer, content equality), or a value struct (N words,
/// memcmp equality unless a str hides inside).
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Word,
    Str,
    Struct { words: usize, has_str: bool },
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

/// One binding's storage: frame offset, value kind, and whether the slot
/// holds a pointer to the value (struct/str params) instead of the value.
#[derive(Clone, Copy)]
struct Local {
    off: i64,
    kind: Kind,
    indirect: bool,
}

struct Emitter<'a> {
    asm: String,
    /// Innermost-last scope stack; each block pushes and pops one frame.
    scopes: Vec<HashMap<String, Local>>,
    /// Bump-allocated frame offsets; `min_slot` is the high-water mark
    /// that sizes the frame (emitted after the body, spliced before it).
    next_slot: i64,
    min_slot: i64,
    /// Global label counter — jump labels must be unique per file.
    labels: usize,
    /// Outstanding operand pushes. %rsp sits 16-aligned at statement
    /// level, so this parity decides the call-site alignment fix-up.
    depth: usize,
    /// The module whose function is being emitted — call names resolve
    /// through its alias map.
    module: usize,
    /// Current function's return size; >1 means sret (hidden dest ptr).
    ret_words: usize,
    /// Frame slot holding the incoming sret destination pointer.
    sret_slot: Option<i64>,
    res: &'a Resolutions,
    /// Read-only data: string literal bytes (deduplicated) — and their
    /// descriptors, which need load-time relocations (.data.rel.ro).
    data: String,
    relro: String,
    str_ids: HashMap<String, usize>,
}

impl Emitter<'_> {
    /// A checker type's backend kind. `None` = not compilable yet
    /// (floats, value optionals) or infinite (recursive value struct —
    /// the fuel guard catches it; values that deep can't exist).
    fn kind_of(&self, t: &Type, fuel: usize) -> Option<Kind> {
        match t {
            Type::Int | Type::Bool => Some(Kind::Word),
            // A nullable handle is a word only if the handle itself is a
            // compilable word (an `int?[]?` must stay as gated as `int?`).
            Type::Optional(inner) => (ref_shaped(inner, self.res)
                && self.kind_of(inner, fuel.checked_sub(1)?)? == Kind::Word)
                .then_some(Kind::Word),
            // Elements must be single words (ADR 0014's stride seat); a
            // value-optional element would alias 0 with null.
            Type::Array(inner) => {
                (self.kind_of(inner, fuel.checked_sub(1)?)? == Kind::Word).then_some(Kind::Word)
            }
            Type::Str => Some(Kind::Str),
            Type::Struct(m, n) => {
                let def = &self.res.structs[&(*m, n.clone())];
                if def.by_ref {
                    return Some(Kind::Word);
                }
                let next = fuel.checked_sub(1)?;
                let mut words = 0;
                let mut has_str = false;
                for (_, ft) in &def.fields {
                    match self.kind_of(ft, next)? {
                        Kind::Word => words += 1,
                        Kind::Str => {
                            words += 2;
                            has_str = true;
                        }
                        Kind::Struct {
                            words: w,
                            has_str: h,
                        } => {
                            words += w;
                            has_str |= h;
                        }
                    }
                }
                Some(Kind::Struct { words, has_str })
            }
            _ => None,
        }
    }

    fn type_words(&self, t: &Type, fuel: usize) -> Option<usize> {
        self.kind_of(t, fuel).map(Kind::words)
    }

    /// A callee's return kind, from the checker's signature table.
    /// Word for unit and for anything not yet compilable (those callees
    /// fail their own gates before any call site matters).
    fn ret_kind_of(&self, key: &(usize, String)) -> Kind {
        self.res
            .sigs
            .get(key)
            .and_then(|sig| self.kind_of(&sig.ret, FUEL))
            .unwrap_or(Kind::Word)
    }

    /// `kind_of` for annotations, resolved through `module`'s names.
    fn ann_kind(&self, ty: &TypeAnn, module: usize) -> Option<Kind> {
        match ty {
            TypeAnn::Int | TypeAnn::Bool => Some(Kind::Word),
            TypeAnn::Str => Some(Kind::Str),
            // Element annotations must be single words: `int?[]` is as
            // uncompilable as a bare `int?`, `Point[]` needs strides.
            TypeAnn::Array(inner) => {
                (self.ann_kind(inner, module)? == Kind::Word).then_some(Kind::Word)
            }
            TypeAnn::Optional(inner) => match inner.as_ref() {
                TypeAnn::Named(n) => self.res.ref_structs[module]
                    .contains(n)
                    .then_some(Kind::Word),
                // The Array arm validates the element type too.
                TypeAnn::Array(_) => self.ann_kind(inner, module),
                _ => None,
            },
            TypeAnn::Named(n) => {
                let key = self.res.types[module].get(n)?;
                self.kind_of(&Type::Struct(key.0, key.1.clone()), FUEL)
            }
            _ => None,
        }
    }

    /// Byte offset of field `index` in `def` — the sum of the sizes
    /// before it (C-style declaration-order layout, ADR 0009).
    fn offset_of(&self, base: &(usize, String), index: usize) -> Option<i64> {
        let def = &self.res.structs[base];
        def.fields[..index].iter().try_fold(0, |sum, (_, ft)| {
            Some(sum + 8 * self.type_words(ft, FUEL)? as i64)
        })
    }

    /// An expression's backend kind, straight from the checker's
    /// per-expression type table — codegen never re-derives a type.
    /// (The Word fallback covers spans the checker never typed, which a
    /// checked program only produces for the `null` literal.)
    fn expr_kind(&self, e: &Expr) -> Kind {
        self.res
            .expr_types
            .get(&e.span())
            .and_then(|t| self.kind_of(t, FUEL))
            .unwrap_or(Kind::Word)
    }

    /// Emits one function: body first into a side buffer (so the frame
    /// size is simply the slot high-water mark), then label + prologue +
    /// body. The fall-through epilogue is reachable only for unit
    /// functions — the checker proves value-returning bodies return.
    fn function(&mut self, f: &Function, module: usize) -> Result<(), Diagnostic> {
        self.module = module;
        self.next_slot = 0;
        self.min_slot = 0;
        self.depth = 0;
        let key = (module, f.name.clone());
        if f.return_type.is_some() {
            let sig = &self.res.sigs[&key];
            if self.kind_of(&sig.ret, FUEL).is_none() {
                return Err(unsupported("this return type", f.span));
            }
        }
        let ret_kind = self.ret_kind_of(&key);
        self.ret_words = ret_kind.words();
        let sret = ret_kind != Kind::Word;
        if f.params.len() + sret as usize > ARG_REGS.len() {
            return Err(unsupported("more than 6 parameters", f.span));
        }

        let outer = std::mem::take(&mut self.asm);

        // The hidden sret pointer arrives first and hides in the frame.
        self.sret_slot = None;
        if sret {
            let slot = self.alloc(1);
            let _ = writeln!(self.asm, "\tmovq %rdi, {slot}(%rbp)");
            self.sret_slot = Some(slot);
        }
        // Params: scalars and handles arrive by value, value structs as
        // pointers to the caller's storage (read-only by checker rule).
        let mut params = HashMap::new();
        let param_types = self.res.sigs[&key].params.clone();
        for (i, (p, ty)) in f.params.iter().zip(&param_types).enumerate() {
            let kind = self
                .kind_of(ty, FUEL)
                .ok_or_else(|| unsupported("parameters of this type", f.span))?;
            let slot = self.alloc(1);
            let reg = ARG_REGS[i + sret as usize];
            let _ = writeln!(self.asm, "\tmovq {reg}, {slot}(%rbp)");
            params.insert(
                p.name.clone(),
                Local {
                    off: slot,
                    kind,
                    indirect: kind != Kind::Word,
                },
            );
        }
        self.scopes = vec![params];

        for stmt in &f.body {
            self.stmt(stmt)?;
        }
        self.asm.push_str("\tleave\n\tret\n");
        let body = std::mem::replace(&mut self.asm, outer);

        let label = label_of(module, &f.name);
        if label == "main" {
            self.asm.push_str("\t.globl main\n");
        }
        // Slots are rbp-relative because operand pushes move %rsp; the
        // frame is 16-byte aligned so %rsp parity at any point is just
        // the outstanding push count.
        let _ = writeln!(self.asm, "{label}:\n\tpushq %rbp\n\tmovq %rsp, %rbp");
        let frame = ((-self.min_slot) + 15) & !15;
        if frame > 0 {
            let _ = writeln!(self.asm, "\tsubq ${frame}, %rsp");
        }
        self.asm.push_str(&body);
        Ok(())
    }

    /// Claims `words` fresh frame words; freed by restoring `next_slot`.
    fn alloc(&mut self, words: usize) -> i64 {
        self.next_slot -= 8 * words as i64;
        self.min_slot = self.min_slot.min(self.next_slot);
        self.next_slot
    }

    fn fresh_label(&mut self) -> String {
        self.labels += 1;
        format!(".L{}", self.labels)
    }

    fn push(&mut self, reg: &str) {
        self.depth += 1;
        let _ = writeln!(self.asm, "\tpushq {reg}");
    }

    fn pop(&mut self, reg: &str) {
        self.depth -= 1;
        let _ = writeln!(self.asm, "\tpopq {reg}");
    }

    /// Emits a call with the ABI's 16-byte %rsp alignment: an odd number
    /// of pending operand pushes leaves %rsp 8 off, fixed up around the
    /// call. Used for user functions, the runtime, and libc alike.
    fn call(&mut self, symbol: &str) {
        let misaligned = self.depth % 2 == 1;
        if misaligned {
            self.asm.push_str("\tsubq $8, %rsp\n");
        }
        let _ = writeln!(self.asm, "\tcall {symbol}");
        if misaligned {
            self.asm.push_str("\taddq $8, %rsp\n");
        }
    }

    /// Copies `words` 8-byte words from %rsi to %rdi (clobbers %rcx and
    /// advances %rsi/%rdi). Only emitted where no operand relies on them.
    fn copy(&mut self, words: usize) {
        let _ = writeln!(self.asm, "\tmovq ${words}, %rcx\n\trep movsq");
    }

    /// Stores %rax into the memory operand `dst`, dispatching on kind —
    /// words move inline, pointer-valued values (structs, strings) copy
    /// their storage. The one store path every consumer shares.
    fn store(&mut self, kind: Kind, dst: &str) {
        if kind == Kind::Word {
            let _ = writeln!(self.asm, "\tmovq %rax, {dst}");
        } else {
            let _ = writeln!(self.asm, "\tleaq {dst}, %rdi\n\tmovq %rax, %rsi");
            self.copy(kind.words());
        }
    }

    /// ADR 0008's runtime bounds check: index in %rcx against the length
    /// of the array whose handle is in `hdl`. Unsigned compare catches
    /// negatives; out of bounds aborts — the deferred-trap policy (like
    /// SIGFPE for idiv), never a silent wild access.
    fn bounds_check(&mut self, hdl: &str) {
        let ok = self.fresh_label();
        let _ = writeln!(self.asm, "\tcmpq 0({hdl}), %rcx\n\tjb {ok}");
        self.call("abort@PLT");
        let _ = writeln!(self.asm, "{ok}:");
    }

    fn lookup(&self, name: &str) -> Option<Local> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn block(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        self.scopes.push(HashMap::new());
        let saved_slot = self.next_slot;
        let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
        // The block's names died with its scope, so its slots are free
        // for sibling blocks.
        self.next_slot = saved_slot;
        self.scopes.pop();
        result
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Diagnostic> {
        // Expression temporaries (value-struct literals, sret call slots)
        // live to the end of their statement; `let` keeps its binding.
        let saved_slot = self.next_slot;
        match stmt {
            Stmt::Let {
                name, value, ty, ..
            } => {
                if let Some(ann) = ty {
                    if self.ann_kind(ann, self.module).is_none() {
                        return Err(unsupported("bindings of this type", stmt.span()));
                    }
                }
                let kind = self.expr_kind(value);
                let off = self.alloc(kind.words());
                self.expr(value)?;
                // Dispatch on kind, never on width: a one-field value
                // struct is one word wide but still pointer-valued.
                self.store(kind, &format!("{off}(%rbp)"));
                self.scopes
                    .last_mut()
                    .expect("a scope is always open")
                    .insert(
                        name.clone(),
                        Local {
                            off,
                            kind,
                            indirect: false,
                        },
                    );
                // Free the value's temporaries, keep the binding.
                self.next_slot = off;
                return Ok(());
            }
            Stmt::Assign { target, value, .. } => match target {
                Expr::Ident(name, span) => {
                    let Some(local) = self.lookup(name) else {
                        return Err(unsupported("this assignment target", *span));
                    };
                    self.expr(value)?;
                    self.store(local.kind, &format!("{}(%rbp)", local.off));
                }
                Expr::Index { base, index, .. } => {
                    // The oracle evaluates the value BEFORE the target's
                    // base and index — side effects must interleave the
                    // same way.
                    self.expr(value)?;
                    self.push("%rax");
                    self.expr(base)?;
                    self.push("%rax");
                    self.expr(index)?;
                    self.asm.push_str("\tmovq %rax, %rcx\n");
                    self.pop("%rdx");
                    self.pop("%rax");
                    self.bounds_check("%rdx");
                    self.asm
                        .push_str("\tmovq 16(%rdx), %rdx\n\tmovq %rax, (%rdx,%rcx,8)\n");
                }
                // `?.` links are not places (the parser rejects them), so
                // a field target is always a plain, checker-proven-safe
                // dereference: no null check needed.
                Expr::Field { base, span, .. } => {
                    let Some(slot) = self.res.field_slots.get(span) else {
                        return Err(unsupported("this field target", *span));
                    };
                    let kind = self
                        .kind_of(&slot.ty, FUEL)
                        .ok_or_else(|| unsupported("fields of this type", *span))?;
                    let off = self
                        .offset_of(&slot.base, slot.index)
                        .ok_or_else(|| unsupported("this struct layout", *span))?;
                    // Value before target base — the oracle's order.
                    self.expr(value)?;
                    self.push("%rax");
                    self.expr(base)?;
                    self.asm.push_str("\tmovq %rax, %rdi\n");
                    self.pop("%rax");
                    self.store(kind, &format!("{off}(%rdi)"));
                }
                other => return Err(unsupported("this assignment target", other.span())),
            },
            Stmt::Return { value, .. } => {
                if let Some(expr) = value {
                    self.expr(expr)?;
                    if let Some(sret) = self.sret_slot {
                        // Copy the value into the caller's destination
                        // and hand the destination back.
                        let _ = writeln!(self.asm, "\tmovq {sret}(%rbp), %rdi\n\tmovq %rax, %rsi");
                        self.copy(self.ret_words);
                        let _ = writeln!(self.asm, "\tmovq {sret}(%rbp), %rax");
                    }
                }
                // `leave` restores %rsp from %rbp, so pending operand
                // pushes on this path unwind with the frame.
                self.asm.push_str("\tleave\n\tret\n");
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                let end = self.fresh_label();
                self.expr(cond)?;
                self.asm.push_str("\ttestq %rax, %rax\n");
                match else_body {
                    None => {
                        let _ = writeln!(self.asm, "\tje {end}");
                        self.block(then_body)?;
                    }
                    Some(else_body) => {
                        let otherwise = self.fresh_label();
                        let _ = writeln!(self.asm, "\tje {otherwise}");
                        self.block(then_body)?;
                        let _ = writeln!(self.asm, "\tjmp {end}\n{otherwise}:");
                        self.block(else_body)?;
                    }
                }
                let _ = writeln!(self.asm, "{end}:");
            }
            Stmt::While { cond, body, .. } => {
                let top = self.fresh_label();
                let end = self.fresh_label();
                let _ = writeln!(self.asm, "{top}:");
                self.expr(cond)?;
                self.asm.push_str("\ttestq %rax, %rax\n");
                let _ = writeln!(self.asm, "\tje {end}");
                self.block(body)?;
                let _ = writeln!(self.asm, "\tjmp {top}\n{end}:");
            }
            Stmt::For {
                index,
                name,
                iterable,
                body,
                ..
            } => {
                // The iterable evaluates once; iteration is live — length
                // re-read every step, element copied out before the body
                // runs — exactly the oracle's contract (interpreter.rs).
                // Three hidden slots: handle, counter (doubling as the
                // index binding), element.
                self.expr(iterable)?;
                let hdl = self.alloc(1);
                let i = self.alloc(1);
                let x = self.alloc(1);
                let _ = writeln!(self.asm, "\tmovq %rax, {hdl}(%rbp)\n\tmovq $0, {i}(%rbp)");
                let top = self.fresh_label();
                let end = self.fresh_label();
                let _ = writeln!(
                    self.asm,
                    "{top}:\n\tmovq {hdl}(%rbp), %rax\n\tmovq {i}(%rbp), %rcx\n\
                     \tcmpq 0(%rax), %rcx\n\tjae {end}\n\
                     \tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax\n\
                     \tmovq %rax, {x}(%rbp)"
                );
                let mut bindings = HashMap::new();
                bindings.insert(
                    name.clone(),
                    Local {
                        off: x,
                        kind: Kind::Word,
                        indirect: false,
                    },
                );
                if let Some(index) = index {
                    bindings.insert(
                        index.clone(),
                        Local {
                            off: i,
                            kind: Kind::Word,
                            indirect: false,
                        },
                    );
                }
                self.scopes.push(bindings);
                let inner_slot = self.next_slot;
                let result = body.iter().try_for_each(|stmt| self.stmt(stmt));
                self.next_slot = inner_slot;
                self.scopes.pop();
                result?;
                let _ = writeln!(self.asm, "\tincq {i}(%rbp)\n\tjmp {top}\n{end}:");
            }
            Stmt::Expr(expr) => self.expr(expr)?, // value discarded
        }
        self.next_slot = saved_slot;
        Ok(())
    }

    /// Emits code leaving the expression's value in %rax — scalars and
    /// handles by value (bools are 0/1), value structs as a pointer to
    /// their storage. Binary ops park the left operand on the machine
    /// stack while the right side evaluates, then pop it into %rcx —
    /// pushes and pops always balance across every emitted path.
    /// Recursion depth is safe: the parser bounds AST height at
    /// construction (MAX_FN_OPS) and the pipeline runs on a worker stack
    /// sized for that bound (main.rs).
    fn expr(&mut self, expr: &Expr) -> Result<(), Diagnostic> {
        match expr {
            // movabsq takes a full 64-bit immediate; movq would cap at i32.
            Expr::Int(n, _) => {
                let _ = writeln!(self.asm, "\tmovabsq ${n}, %rax");
            }
            Expr::Bool(b, _) => {
                let _ = writeln!(self.asm, "\tmovq ${}, %rax", *b as i64);
            }
            // `null` is handle 0 — sound because value-typed optionals
            // never compile (annotation, param, field, and array-literal
            // gates), so 0 is never a legitimate optional payload.
            Expr::Null(_) => {
                self.asm.push_str("\txorl %eax, %eax\n");
            }
            // A literal's descriptor and bytes are both static — strings
            // are immutable, so every use shares one rodata object.
            Expr::Str(text, _) => {
                let id = self.str_id(text);
                let _ = writeln!(self.asm, "\tleaq .Lsd{id}(%rip), %rax");
            }
            Expr::Ident(name, span) => match self.lookup(name) {
                Some(local) if local.kind == Kind::Word || local.indirect => {
                    let _ = writeln!(self.asm, "\tmovq {}(%rbp), %rax", local.off);
                }
                // A by-value struct/str local: its value IS the frame slots.
                Some(local) => {
                    let _ = writeln!(self.asm, "\tleaq {}(%rbp), %rax", local.off);
                }
                // The checker resolved it, but not to a local we can
                // compile yet (e.g. a function name used as a value).
                None => return Err(unsupported("this name", *span)),
            },
            Expr::Unary { op, rhs, .. } => {
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => self.asm.push_str("\tnegq %rax\n"),
                    // Bools are exactly 0 or 1, so `!` is one bit flip.
                    UnOp::Not => self.asm.push_str("\txorq $1, %rax\n"),
                }
            }
            // The three short-circuit operators share one shape: the left
            // side IS the result when it decides (false for &&, true for
            // ||, non-null for ?? — null is handle 0, value optionals
            // never compile), and the right side stays lazy, like the
            // oracle — its traps and effects must stay unreached.
            Expr::Binary {
                op: op @ (BinOp::And | BinOp::Or | BinOp::Coalesce),
                lhs,
                rhs,
                ..
            } => {
                let jcc = if matches!(op, BinOp::And) {
                    "je"
                } else {
                    "jne"
                };
                let end = self.fresh_label();
                self.expr(lhs)?;
                let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\t{jcc} {end}");
                self.expr(rhs)?;
                let _ = writeln!(self.asm, "{end}:");
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let kind = self.expr_kind(lhs);
                // Concatenation: the one explicitly allocating string
                // operation (ADR 0013) — new buffer, both byte runs
                // copied, fresh descriptor in a statement temp.
                if kind == Kind::Str && matches!(op, BinOp::Add) {
                    let l = self.alloc(1);
                    let r = self.alloc(1);
                    let buf = self.alloc(1);
                    let out = self.alloc(2);
                    self.expr(lhs)?;
                    let _ = writeln!(self.asm, "\tmovq %rax, {l}(%rbp)");
                    self.expr(rhs)?;
                    let _ = writeln!(self.asm, "\tmovq %rax, {r}(%rbp)");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq {l}(%rbp), %rax\n\tmovq 8(%rax), %rdi\n\
                         \tmovq {r}(%rbp), %rax\n\taddq 8(%rax), %rdi"
                    );
                    self.call("malloc@PLT");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq %rax, {buf}(%rbp)\n\tmovq %rax, %rdi\n\
                         \tmovq {l}(%rbp), %rax\n\tmovq 0(%rax), %rsi\n\tmovq 8(%rax), %rdx"
                    );
                    self.call("memcpy@PLT");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq {buf}(%rbp), %rdi\n\tmovq {l}(%rbp), %rax\n\
                         \taddq 8(%rax), %rdi\n\
                         \tmovq {r}(%rbp), %rax\n\tmovq 0(%rax), %rsi\n\tmovq 8(%rax), %rdx"
                    );
                    self.call("memcpy@PLT");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq {buf}(%rbp), %rax\n\tmovq %rax, {out}(%rbp)\n\
                         \tmovq {l}(%rbp), %rax\n\tmovq 8(%rax), %rcx\n\
                         \tmovq {r}(%rbp), %rax\n\taddq 8(%rax), %rcx\n\
                         \tmovq %rcx, {}(%rbp)\n\tleaq {out}(%rbp), %rax",
                        out + 8
                    );
                    return Ok(());
                }
                if matches!(op, BinOp::Eq | BinOp::Ne) && kind != Kind::Word {
                    // The right side may mutate the left side's storage
                    // through an alias, and the oracle compares the value
                    // from before — so snapshot the left operand first.
                    match kind {
                        // Content equality (ADR 0013): length first, then
                        // the bytes. The checker guarantees both sides
                        // share one type.
                        Kind::Str => {
                            self.expr(lhs)?;
                            let snap = self.alloc(2);
                            let _ =
                                writeln!(self.asm, "\tleaq {snap}(%rbp), %rdi\n\tmovq %rax, %rsi");
                            self.copy(2);
                            self.expr(rhs)?;
                            let _ = writeln!(self.asm, "\tleaq {snap}(%rbp), %rcx");
                            let differ = self.fresh_label();
                            let end = self.fresh_label();
                            let _ = writeln!(
                                self.asm,
                                "\tmovq 8(%rcx), %rdx\n\tcmpq 8(%rax), %rdx\n\tjne {differ}\n\
                                 \tmovq 0(%rcx), %rdi\n\tmovq 0(%rax), %rsi"
                            );
                            self.call("memcmp@PLT");
                            let _ = writeln!(
                                self.asm,
                                "\ttestl %eax, %eax\n\tsete %al\n\tmovzbq %al, %rax\n\
                                 \tjmp {end}\n{differ}:\n\txorl %eax, %eax\n{end}:"
                            );
                        }
                        // A str field inside means one memcmp would
                        // compare descriptors, not content.
                        Kind::Struct { has_str: true, .. } => {
                            return Err(unsupported("'==' on structs containing strings", *span));
                        }
                        // Value-struct equality is structural: the layout
                        // is padding-free 8-byte words, so memcmp decides.
                        Kind::Struct { words, .. } => {
                            self.expr(lhs)?;
                            let snap = self.alloc(words);
                            let _ =
                                writeln!(self.asm, "\tleaq {snap}(%rbp), %rdi\n\tmovq %rax, %rsi");
                            self.copy(words);
                            self.expr(rhs)?;
                            let _ = writeln!(
                                self.asm,
                                "\tmovq %rax, %rsi\n\tleaq {snap}(%rbp), %rdi\n\tmovq ${}, %rdx",
                                8 * words
                            );
                            self.call("memcmp@PLT");
                            let _ = writeln!(
                                self.asm,
                                "\ttestl %eax, %eax\n\tsete %al\n\tmovzbq %al, %rax"
                            );
                        }
                        Kind::Word => unreachable!(),
                    }
                    if matches!(op, BinOp::Ne) {
                        self.asm.push_str("\txorq $1, %rax\n");
                    }
                    return Ok(());
                }
                // lhs in %rax, rhs in %rcx. Wrapping add/sub/mul match the
                // interpreter's wrapping ops; idiv truncates toward zero
                // and signs the remainder like the dividend, matching the
                // oracle on every input it runs cleanly (it diagnoses the
                // idiv traps: /0 and i64::MIN / -1).
                self.expr(lhs)?;
                self.push("%rax");
                self.expr(rhs)?;
                self.asm.push_str("\tmovq %rax, %rcx\n");
                self.pop("%rax");
                match op {
                    BinOp::Add => self.asm.push_str("\taddq %rcx, %rax\n"),
                    BinOp::Sub => self.asm.push_str("\tsubq %rcx, %rax\n"),
                    BinOp::Mul => self.asm.push_str("\timulq %rcx, %rax\n"),
                    BinOp::Div => self.asm.push_str("\tcqto\n\tidivq %rcx\n"),
                    BinOp::Rem => self
                        .asm
                        .push_str("\tcqto\n\tidivq %rcx\n\tmovq %rdx, %rax\n"),
                    // cmpq computes rax - rcx, so the condition code reads
                    // lhs ? rhs; one shared template keeps the six
                    // comparisons typo-proof.
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match op {
                            BinOp::Eq => "e",
                            BinOp::Ne => "ne",
                            BinOp::Lt => "l",
                            BinOp::Le => "le",
                            BinOp::Gt => "g",
                            BinOp::Ge => "ge",
                            _ => unreachable!(),
                        };
                        let _ = writeln!(
                            self.asm,
                            "\tcmpq %rcx, %rax\n\tset{cc} %al\n\tmovzbq %al, %rax"
                        );
                    }
                    BinOp::And | BinOp::Or | BinOp::Coalesce => {
                        unreachable!("handled above")
                    }
                }
            }
            Expr::Call { callee, args, span } => {
                let Expr::Ident(name, _) = callee.as_ref() else {
                    return Err(unsupported("this callee", *span));
                };
                let Some(key) = self.res.functions[self.module].get(name) else {
                    // Resolution order says: no user definition, so this
                    // is a builtin.
                    return self.builtin(name, args, *span);
                };
                let ret_kind = self.ret_kind_of(key);
                let sret = ret_kind != Kind::Word;
                if args.len() + sret as usize > ARG_REGS.len() {
                    return Err(unsupported("calls with more than 6 arguments", *span));
                }
                // A struct-returning callee writes into a fresh temp here
                // in the caller's frame (freed at statement end).
                let dest = sret.then(|| self.alloc(ret_kind.words()));
                // Evaluate args left to right onto the stack (a later
                // arg's subexpressions would clobber earlier registers),
                // then pop into the ABI registers in reverse. Multi-word
                // values are copied into private temps AT EVALUATION TIME:
                // the oracle copies arguments as it evaluates them, so a
                // later arg mutating the storage through an alias must not
                // be visible (and the callee's storage stays immutable).
                for arg in args {
                    self.expr(arg)?;
                    let kind = self.expr_kind(arg);
                    if kind != Kind::Word {
                        let tmp = self.alloc(kind.words());
                        let _ = writeln!(self.asm, "\tleaq {tmp}(%rbp), %rdi\n\tmovq %rax, %rsi");
                        self.copy(kind.words());
                        let _ = writeln!(self.asm, "\tleaq {tmp}(%rbp), %rax");
                    }
                    self.push("%rax");
                }
                let shifted = &ARG_REGS[sret as usize..sret as usize + args.len()];
                for reg in shifted.iter().rev() {
                    self.pop(reg);
                }
                if let Some(dest) = dest {
                    let _ = writeln!(self.asm, "\tleaq {dest}(%rbp), %rdi");
                }
                self.call(&label_of(key.0, &key.1));
            }
            Expr::ArrayLit { elements, .. } => {
                // A null element could make the literal an `int?[]` —
                // a value-optional array the word model can't represent.
                // ponytail: over-strict for `Node?[]` literals too; build
                // those with push until the checker exports element types.
                if let Some(null) = elements.iter().find(|e| matches!(e, Expr::Null(_))) {
                    return Err(unsupported(
                        "array literals with null elements",
                        null.span(),
                    ));
                }
                if let Some(wide) = elements.iter().find(|e| self.expr_kind(e) != Kind::Word) {
                    return Err(unsupported("arrays of multi-word values", wide.span()));
                }
                // Header {len, cap, data*} plus buffer, per ADR 0014.
                // Allocation happens before element evaluation — the
                // ordering difference from the oracle is only observable
                // through traps, which the harness excludes.
                let n = elements.len();
                self.asm.push_str("\tmovq $24, %rdi\n");
                self.call("malloc@PLT");
                self.push("%rax");
                let _ = writeln!(self.asm, "\tmovq ${}, %rdi", 8 * n.max(1));
                self.call("malloc@PLT");
                self.pop("%rcx");
                let _ = writeln!(
                    self.asm,
                    "\tmovq %rax, 16(%rcx)\n\tmovq ${n}, 0(%rcx)\n\tmovq ${n}, 8(%rcx)"
                );
                for (i, element) in elements.iter().enumerate() {
                    self.push("%rcx");
                    self.expr(element)?;
                    self.pop("%rcx");
                    let _ = writeln!(
                        self.asm,
                        "\tmovq 16(%rcx), %rdx\n\tmovq %rax, {}(%rdx)",
                        8 * i
                    );
                }
                self.asm.push_str("\tmovq %rcx, %rax\n");
            }
            Expr::Index { base, index, .. } => {
                self.expr(base)?;
                self.push("%rax");
                self.expr(index)?;
                self.asm.push_str("\tmovq %rax, %rcx\n");
                self.pop("%rax");
                self.bounds_check("%rax");
                self.asm
                    .push_str("\tmovq 16(%rax), %rax\n\tmovq (%rax,%rcx,8), %rax\n");
            }
            Expr::Field {
                base,
                optional,
                span,
                ..
            } => {
                let Some(slot) = self.res.field_slots.get(span) else {
                    return Err(unsupported("this field access", *span));
                };
                let field_kind = self
                    .kind_of(&slot.ty, FUEL)
                    .ok_or_else(|| unsupported("fields of this type", *span))?;
                let off = self
                    .offset_of(&slot.base, slot.index)
                    .ok_or_else(|| unsupported("this struct layout", *span))?;
                // `p?.x` with a value-typed x yields `int?` — a value
                // optional the word model can't represent. Handle-typed
                // fields are fine, already-optional ones stay flat.
                let unwrapped = match &slot.ty {
                    Type::Optional(inner) => inner,
                    other => other,
                };
                if *optional && !ref_shaped(unwrapped, self.res) {
                    return Err(unsupported("'?.' on a field of value type", *span));
                }
                self.expr(base)?;
                if *optional {
                    // Null short-circuits to null (0 stays in %rax).
                    let end = self.fresh_label();
                    let _ = writeln!(self.asm, "\ttestq %rax, %rax\n\tje {end}");
                    let _ = writeln!(self.asm, "\tmovq {off}(%rax), %rax\n{end}:");
                } else if field_kind == Kind::Word {
                    // No null check: the checker's narrowing is sound, so
                    // a plain `.` base is proven non-null (ADR 0007).
                    let _ = writeln!(self.asm, "\tmovq {off}(%rax), %rax");
                } else {
                    // A struct/str-typed field's value is its storage
                    // inside the base — an interior pointer; consumers
                    // copy.
                    let _ = writeln!(self.asm, "\tleaq {off}(%rax), %rax");
                }
            }
            Expr::StructLit { fields, span, .. } => {
                let Some(Type::Struct(dm, dn)) = self.res.expr_types.get(span) else {
                    return Err(unsupported("this struct literal", *span));
                };
                let def = &self.res.structs[&(*dm, dn.clone())];
                let field_kinds: Vec<Kind> = def
                    .fields
                    .iter()
                    .map(|(_, t)| self.kind_of(t, FUEL))
                    .collect::<Option<_>>()
                    .ok_or_else(|| unsupported("structs with fields of this type", *span))?;
                let offsets: Vec<i64> = field_kinds
                    .iter()
                    .scan(0i64, |acc, k| {
                        let off = *acc;
                        *acc += 8 * k.words() as i64;
                        Some(off)
                    })
                    .collect();
                let total: usize = field_kinds.iter().map(|k| k.words()).sum();
                let slot_of = |fname: &str| {
                    def.fields
                        .iter()
                        .position(|(dn, _)| dn == fname)
                        .expect("checker verified the field exists")
                };
                if def.by_ref {
                    // One heap object; the checker proved the literal
                    // complete, so every slot is written.
                    let _ = writeln!(self.asm, "\tmovq ${}, %rdi", (8 * total).max(8));
                    self.call("malloc@PLT");
                    self.push("%rax");
                    for (fname, value) in fields {
                        let i = slot_of(fname);
                        self.expr(value)?;
                        self.pop("%rdx");
                        self.store(field_kinds[i], &format!("{}(%rdx)", offsets[i]));
                        self.push("%rdx");
                    }
                    self.pop("%rax");
                } else {
                    // A value literal builds in a frame temp; its static
                    // address means no handle juggling at all.
                    let base = self.alloc(total.max(1));
                    for (fname, value) in fields {
                        let i = slot_of(fname);
                        self.expr(value)?;
                        self.store(field_kinds[i], &format!("{}(%rbp)", base + offsets[i]));
                    }
                    let _ = writeln!(self.asm, "\tleaq {base}(%rbp), %rax");
                }
            }
            other => return Err(unsupported("this expression", other.span())),
        }
        Ok(())
    }

    /// The compilable builtins. `len` is two loads; `push` calls the
    /// in-assembly runtime (its unit result is whatever's in %rax — the
    /// checker keeps unit values out of operand positions).
    fn builtin(&mut self, name: &str, args: &[Expr], span: Span) -> Result<(), Diagnostic> {
        match (name, args) {
            ("len", [array]) => {
                self.expr(array)?;
                self.asm.push_str("\tmovq 0(%rax), %rax\n");
            }
            ("push", [array, value]) => {
                if self.expr_kind(value) != Kind::Word {
                    return Err(unsupported("arrays of multi-word values", value.span()));
                }
                self.expr(array)?;
                self.push("%rax");
                self.expr(value)?;
                self.asm.push_str("\tmovq %rax, %rsi\n");
                self.pop("%rdi");
                self.call("ys_push");
            }
            ("print", [value]) => {
                let ty = self
                    .res
                    .expr_types
                    .get(&value.span())
                    .ok_or_else(|| unsupported("printing this value", span))?;
                match ty {
                    // printf is varargs: %al must carry the vector-reg
                    // count (0) at every call.
                    Type::Int => {
                        self.expr(value)?;
                        self.asm.push_str(
                            "\tmovq %rax, %rsi\n\tleaq .Lfmt_int(%rip), %rdi\n\txorl %eax, %eax\n",
                        );
                        self.call("printf@PLT");
                    }
                    Type::Bool => {
                        self.expr(value)?;
                        self.asm.push_str(
                            "\ttestq %rax, %rax\n\tleaq .Ltrue_s(%rip), %rsi\n\
                             \tleaq .Lfalse_s(%rip), %rcx\n\tcmoveq %rcx, %rsi\n\
                             \tleaq .Lfmt_cstr(%rip), %rdi\n\txorl %eax, %eax\n",
                        );
                        self.call("printf@PLT");
                    }
                    // Length-carried, so %.*s with (len, ptr).
                    Type::Str => {
                        self.expr(value)?;
                        self.asm.push_str(
                            "\tmovq 8(%rax), %rsi\n\tmovq 0(%rax), %rdx\n\
                             \tleaq .Lfmt_str(%rip), %rdi\n\txorl %eax, %eax\n",
                        );
                        self.call("printf@PLT");
                    }
                    _ => return Err(unsupported("printing values of this type", span)),
                }
            }
            _ => return Err(unsupported(&format!("builtin '{name}'"), span)),
        }
        Ok(())
    }

    /// Interns a string literal in rodata: raw bytes (no terminator, no
    /// escaping pitfalls — emitted as .byte lists) plus an aligned
    /// `{ptr, len}` descriptor. Returns the descriptor's id.
    fn str_id(&mut self, text: &str) -> usize {
        if let Some(&id) = self.str_ids.get(text) {
            return id;
        }
        let id = self.str_ids.len();
        let _ = writeln!(self.data, ".Lsb{id}:");
        for chunk in text.as_bytes().chunks(16) {
            let bytes: Vec<String> = chunk.iter().map(|b| b.to_string()).collect();
            let _ = writeln!(self.data, "\t.byte {}", bytes.join(","));
        }
        let _ = writeln!(
            self.relro,
            "\t.balign 8\n.Lsd{id}:\n\t.quad .Lsb{id}\n\t.quad {}",
            text.len()
        );
        self.str_ids.insert(text.to_string(), id);
        id
    }
}

fn unsupported(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("not yet compilable: {what}"), span)
}
