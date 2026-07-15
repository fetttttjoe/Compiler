# ADR 0025 — Aggregate Printing: Monomorphized Show Routines

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0017 (which names the render contract), ADR 0012
  (law 1: no boxing, no runtime type info)

## Context

`print` accepts any single value (checker) and the interpreter renders
aggregates source-like — `interpreter/render.rs` is the normative
text: struct fields in name-sorted order, Rust-Display scalars, raw
strings, and a depth budget of 8 where a refstruct handle hop costs a
level (cycles bottom out as `...`). The compiled engine diagnoses all
of it. There is no runtime type info to drive a generic renderer, and
the no-boxing law forbids adding any.

## Decisions

1. **One monomorphized routine per printed type**, generated at
   compile time as ordinary IR (`ys.show.N` labels — the dot cannot
   appear in a user identifier, so no collision). Signature
   `(value_or_pointer, depth)`: word-kind values ride in the register,
   multi-word values pass their pointer — exactly how values travel
   everywhere. Routines are registered lazily from `print` sites and
   generated transitively (fields, elements), memoized per type.
2. **The oracle's depth algebra, verbatim.** Budget 8 at the top;
   every routine opens with `depth == 0 → "..."`. Value-struct fields
   and array elements render at `depth - 1`; refstruct fields at
   `depth - 2` with the extra `depth == 1 → "..."` test (the hop).
   Present optional payloads render at the *same* depth — the
   interpreter stores payloads unwrapped, so an optional adds no
   level. Same budget, same bytes.
3. **Name-sorted fields, declaration-order offsets.** The render
   order sorts by field name (interpreter storage order, pinned as
   observable spec); loads still use ADR 0009's declaration-order
   layout.
4. **Handle routines absorb null.** Refstruct and array routines test
   `handle == 0 → "null"` up front, so `T` and ref-shaped `T?` share
   one routine (non-null handles never hit it). Value optionals get a
   tag-testing wrapper routine around the payload's.
5. **Output goes through printf fragments** on the same stdout stream
   as scalar prints: fixed pieces (`"Name { "`, `", field: "`,
   `"]"`, `"null"`, `"..."`) print as their own format strings —
   identifier charsets cannot contain `%` — and leaves use new raw
   formats `%ld` / `%.*s`. Strings render raw (no quotes), exactly
   like the oracle. `print` appends the trailing newline after the
   routine returns.
6. **Floats inside aggregates stay gated** (`printing floats`) until
   float formatting lands — a cycle-safe `contains_float` walk decides
   at the print site. `print` of a unit-typed call becomes the literal
   `unit`, the oracle's text.

## Consequences

**Positive:** the checker/backend print gap closes for structs,
refstructs, arrays, and value-struct optionals; cyclic values print
bounded, byte-identical to the oracle. After this, `not yet
compilable` is float formatting alone.

**Accepted costs:** one printf call per fragment (print is not a hot
path); each printed type adds a routine to the text section; the
depth budget stays a cap, not cycle detection — same trade the
interpreter documents.
