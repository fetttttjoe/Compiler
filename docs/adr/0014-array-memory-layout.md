# ADR 0014 — Array Memory Layout: Inline Elements Behind a Handle

- **Status:** Proposed — design pinned, no codegen yet
- **Date:** 2026-07-08
- **Extends:** ADR 0008 (arrays' reference semantics), ADR 0009 (heap
  story), ADR 0012 (laws 1–3: natural layout, no boxing, aliasing as API)

## Context

`T[]` exists with reference semantics; the interpreter stores
`Vec<Value>` — fine for the oracle, undefined as a compiled layout. Where
elements live decides whether array code runs at C speed (contiguous,
prefetchable, vectorizable) or at Java speed (a pointer chase per
element). Pinned now, before arrays reach codegen.

## Decisions

1. **An array value is a handle: one pointer to a heap header
   `{ len, cap, data* }`.** Handle copies alias (the existing semantics);
   growth reallocates the buffer and updates the header, so every alias
   observes it. `len(a)` is one load.
2. **Elements of value types are stored inline in the buffer** —
   contiguous C layout, stride = the struct's size including padding.
   `Point[]` is `{x,y}{x,y}…`, exactly a C array. Elements of reference
   types (refstruct, arrays) store handles; `str` elements store their
   16-byte fat pointer inline (ADR 0013). Nothing is ever boxed (ADR
   0012 law 1).
3. **Explicit indexing keeps its bounds check** (`a[i]`, per ADR 0008's
   runtime rule). **`for x in xs` needs no extra check:** the oracle pins
   live iteration — length is re-read each step (the body may push) and
   the element is copied out before the body runs — so the loop
   condition `i < len` *is* the bounds check, at zero added cost. The
   per-step element copy is oracle semantics; "large element copied per
   iteration" is a future hint-class diagnostic (ADR 0005).
4. **Growth doubles capacity** (minimum 4) for amortized O(1) `push`;
   the constant is tunable once benchmarks exist (ADR 0012 law 5).
5. **Fixed-size stack arrays (`[T; N]`) are a named seat** — the
   stack-allocated, length-known-at-compile-time complement; own ADR
   when a real program wants it.

## Consequences

**Positive:** arrays of value structs match C arrays — sequential access
is prefetcher- and SIMD-friendly, and the SoA seat (ADR 0012) becomes a
per-type layout swap rather than a semantic change; random access costs
one extra load (header hop), hoistable once the IR exists.

**Accepted costs:** two allocations' worth of indirection (handle →
header → buffer) keeps growth alias-correct — the price of reference
semantics already chosen in ADR 0008; per-iteration element copies in
`for-in` are visible semantics, mitigated later by the hint class and
the IR's copy elision.
