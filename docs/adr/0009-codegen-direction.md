# ADR 0009 — Codegen Direction (Proposed)

- **Status:** Proposed — design pinned, no code yet
- **Date:** 2026-07-07
- **Extends:** ADR 0001 (fills the reserved `codegen` seat), ADR 0005
  (value semantics and the efficiency mandate), ADR 0006/0008 (reference
  types define the heap story)

## Context

The language's stated identity (ADR 0005) is TypeScript surface with C
economics: ahead-of-time compiled native code, efficiency first, with the
compiler surfacing optimization hints. Everything to date runs on the
tree-walking interpreter, which was built as the reference semantics and
differential-testing oracle — not the product. This ADR pins the backend
direction so feature work stops drifting from it.

## Decisions (proposed)

1. **Target: x86-64 Linux first,** as textual assembly (`.s`) assembled
   and linked through the system `cc`. Honors the zero-dependency rule
   (no cranelift/LLVM), keeps output debuggable with standard tools, and
   leaves AArch64 as the second target to force the abstraction honest.
2. **No bytecode VM stage.** The interpreter already owns "portable
   reference semantics"; a VM would be a third implementation of the same
   language. Compilation goes AST → assembly directly, with an IR
   introduced only when a real optimization needs it.
3. **Memory model:** value structs are stack slots / registers with
   C-style layout; refstructs and arrays are heap allocations behind
   pointers; `T?` on a reference type is a nullable pointer (free), on a
   value type a tag word (the documented cost). No GC — the initial
   collector-free story is arena/leak (programs are short-lived), with
   ownership or RC as an explicit later ADR.
4. **The interpreter becomes the oracle.** Every codegen test runs both
   paths and diffs results — the differential harness IS the test suite
   for the backend.
5. **Optimization hints (ADR 0005's seat) activate here:** the hint class
   ships with the backend ("large struct copied per call — consider
   refstruct", "int? pays a tag — use a sentinel if this is hot"),
   because only codegen knows the real costs.

## Consequences

Committing to this direction means: new language features from now on
state their layout/lowering story in their ADR; `Resolutions` (ADR 0004)
grows type exports when codegen needs them (the flagged seat from the
refstruct review); and the first backend milestone is the smallest
end-to-end slice — `main` returning an int, compiled, linked, exit code
diffed against the interpreter — before any breadth.
