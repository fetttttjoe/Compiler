# ADR 0018 — One Backend: the Feature Framework

- **Status:** Accepted
- **Date:** 2026-07-08
- **Extends:** ADR 0016 (whose tier contract promised this retirement),
  ADR 0017 (whose road map this makes cheap to execute)

## Context

A language feature currently costs three implementations: interpreter,
direct emitter, IR tier — and the review record shows the direct
emitter's ad-hoc paths are where miscompiles bred. Before optimizing
further or building features, the platform gets fixed so a feature is
one lowering, not two emitters.

## Decisions

1. **The IR tier becomes the only backend.** Lowering covers the whole
   language: multi-word values (structs, strings) are frame-temp
   pointers in word vregs; field access, literals, sret calls, content
   equality, concatenation, and print all lower to a small closed
   instruction set plus composition. The direct emitter is deleted, and
   with it the fallback path — the `unwrap_or(Word)` hazard class dies
   structurally, because lowering must now answer for every type.
2. **`lower` returns `Result`, not `Option`.** With no fallback, the
   remaining gates (value optionals, multi-word array elements, float
   printing, recursive value structs) are honest diagnostics from the
   one backend.
3. **The review-hardened semantics carry over verbatim** and the whole
   suite — 349 tests, the conformance corpus, the golden outputs — must
   pass unchanged. Zero goldens move.
4. **The feature recipe is documentation** (CLAUDE.md): ADR with layout
   story → checker + type table → interpreter (normative) → lowering →
   conformance corpus files, in one commit chain. Anything skipping a
   step isn't done.
5. **The differential fuzzer moves in-repo** (`tools/fuzz.py`, per ADR
   0012 law 6, whose IR trigger has fired): random programs through both
   engines, stdout and exit codes compared. Run before merging backend
   changes; review agents proved the method (~1000 programs, multiple
   real bugs).

## Consequences

**Positive:** feature cost drops from three implementations to a
lowering plus spec artifacts; miscompile surface shrinks to one
well-tested path; optimization work (SSA, coalescing) now happens in
the only backend instead of the better of two.

**Accepted costs:** the lowering inherits the direct emitter's full
complexity in one place (that's the point); pathological functions no
longer have a simpler fallback, so the liveness pass must scale
(bitset dataflow) instead of being dodged.
