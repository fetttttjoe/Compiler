# ADR 0004 — Module System

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0003 (supersedes its decision 4: the single global
  namespace is replaced by per-file namespaces)

## Context

Handing the compiler a list of files that merge into one namespace is not a
production flow: without imports, multiple files cannot express structure,
and cross-file name collisions were errors by necessity. The language needed
its module story — decided early, because retrofitting privacy and name
resolution breaks users.

## Decisions

1. **File = module; TypeScript-style surface.**
   `export fun` / `export struct` marks the public API (everything else is
   file-private); `import { a, b } from "./path";` binds exported items.
   Paths resolve relative to the importing file, lexically (`.`/`..` folded,
   no filesystem canonicalization); the `.ys` extension is optional in
   import paths.
2. **One file, one namespace.** Local items and imported names share it: any
   collision within a file (local+local, import+local, import+import) is an
   error. The same name in *different* modules coexists freely. Import
   errors distinguish "module has no item 'x'" from "'x' exists but is not
   exported".
3. **Entry-point-driven compilation.** `compiler <entry.ys>` parses the
   entry, discovers imports wave-by-wave (each wave lexed+parsed in
   parallel, bounded workers as per ADR 0003), and closes the graph. The
   import graph *is* the project definition; a `project.toml` manifest
   remains a named seat feeding the same loader. The entry file must define
   `main`.
4. **Import cycles are rejected** (Go's rule) with the cycle path in the
   error (`import cycle: a.ys → b.ys → a.ys`). The graph stays a DAG — the
   property parallel scheduling and future incremental rebuilds depend on.
5. **Struct identity is `(module, name)`.** Same-named structs in different
   modules are distinct types; passing `a.P` where `b.P` is expected is a
   type error. (Diagnostic phrasing for that case still prints the short
   name twice — module-qualified names in messages are a noted follow-up.)
6. **Resolution is the checker's durable output.** `check(&ModuleGraph)
   -> (Resolutions, Vec<Diagnostic>)` where `Resolutions` maps, per module,
   every visible callable name to its defining `(module, name)`. The
   interpreter executes against it (calls resolve in the *callee's* module
   context, threaded through call frames); codegen will consume the same
   structure. This replaces ADR 0001's `SymbolTable` as the check contract.
7. **Loader is filesystem-abstract.** `load_program(entry, read, map)`
   takes a reader function, so module tests run on in-memory files; only
   `main.rs` touches the disk.

## Consequences

**Positive:** real project structure (privacy, explicit dependencies,
same-name freedom across files); the import DAG needed for incremental
rebuilds now exists; discovery parallelism scales with the graph's width;
all resolution logic is in one phase with the interpreter consuming its
output rather than re-deriving it.

**Accepted costs:** no namespace imports (`import * as m`) or re-exports
yet — named seats; cycle detection DFS recurses (graph-depth stack; fine for
real projects, notable only for pathological chains); the not-exported /
distinct-struct diagnostics could carry richer context (module-qualified
type names, "did you mean to export?").

**Deferred (named seats):** `import * as ns`, re-exports, `export` for
future top-level constants, `project.toml`, incremental rebuilds over the
import DAG, module-qualified names in diagnostics.
