# ADR 0003 — Multi-File Compilation and Parallel Front-End

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0001 (lands the "CLI file input" seat; scales the pipeline
  to many files)

## Context

The language targets big projects: the compiler must analyze many files fast
(parallel front-end) and reliably (deterministic output), with an eye toward
incremental rebuilds. The single hardcoded-source driver had to become a real
multi-file CLI without disturbing the phase contracts.

## Decisions

1. **Global-offset `SourceMap`** (the Go `token.FileSet` / rustc `SourceMap`
   model). Every file gets a base offset in one address space; `Span` stays
   `{ start, end }` and no downstream phase learns about files — only the
   lexer takes a base (`lex_at`) and the renderer resolves offsets
   (`SourceMap::resolve`) at diagnostic time. Adjacent files get a **+1 base
   gap** (as in Go) so a zero-width EOF span can never collide with the next
   file's first byte.
2. **Hard-won caveat, recorded now (rustc's incremental pain):** absolute
   offsets shift when an earlier file changes length. Therefore **future
   caches must never be keyed on absolute spans** — key on file content hash
   plus file-local offsets, resolving through the `SourceMap` at boundaries.
   Stable persisted bases (Go-style slots) are the escape hatch if needed.
3. **Parallel front-end, bounded.** Lex+parse run per file inside
   `std::thread::scope`, safe by construction: phases are pure functions and
   ASTs are owned/`Send`. Files are **chunked across at most
   `available_parallelism()` workers** — never thread-per-file, which falls
   over at project scale. std only; no rayon until profiling demands it.
4. **Whole-program check over merged ASTs — one namespace.** Item names are
   unique across the entire program for now (a duplicate `fun f` in another
   file is an error): without imports there is no way to disambiguate a call,
   so last-one-wins would be a silent footgun. Per-file/module namespaces
   arrive with the module system (own design round).
5. **Determinism is a contract.** Front-end diagnostics sort by
   `(span.start, span.end)` (stable sort) before reporting — same input,
   byte-identical output, regardless of thread scheduling. Item merge order
   is CLI argument order; program semantics are order-independent because
   signatures are collected before bodies (ADR 0001).
6. **CLI contract.** `compiler <file>...`; exit 0 = ran, 1 = compile/runtime/
   IO error, 2 = usage error. Unreadable files are clean errors, never
   panics. Source files use the **`.ys`** extension. End-to-end CLI behavior
   is tested from outside via `tests/cli.rs` (`CARGO_BIN_EXE_`) — the one
   deliberate exception to inline-only tests.

## Consequences

**Positive:** multi-file programs with cross-file calls work today; the
parallel scaffold exists and is proven safe by ownership, not by care;
diagnostics carry file names; adding the module system later changes name
*resolution*, not this plumbing.

**Accepted costs:** one global namespace until modules land; absolute-span
cache keys are forbidden (decision 2); the crate/binary name `Compiler`
predates the language's name and is embedded in `tests/cli.rs`'s
`CARGO_BIN_EXE_Compiler` — renaming the package later touches that one spot.

**Deferred (named seats):** `project.toml` manifest (feeds the same
`SourceMap` loading path); imports/modules/visibility + cycle policy (own
brainstorm); incremental rebuilds (import DAG + per-file content hashes);
per-function parallel checking (`check_function` already reads
`&SymbolTable` immutably); string interning (only with profiling evidence).
