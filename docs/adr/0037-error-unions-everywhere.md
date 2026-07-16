# ADR 0037 — Error Unions Everywhere: Params, Fields, Elements, Payloads

- **Status:** Accepted
- **Date:** 2026-07-16
- **Extends:** 0034 (which promised this widening and built the
  position-independent representation it rides), 0026 (the equality
  doctrine the ban below completes), 0035/0036 (whose instances now
  carry `T!` like any other type)

## Context

ADR 0034 shipped `T!` in the flow positions — returns and
`var`/`const` — and gated params, struct fields, and array elements
behind "not yet supported here" diagnostics, promising a mechanical
widening ("the representation is position-independent"). ADR 0036
added the same gate for enum payloads. The pull has arrived: generics
and payload enums exist, so `Pair<int!, string>` and
`enum E { V(int!) }` are writable up to the gate, and error-carrying
data structures (a parse result stored in a field, a batch of `T!`
outcomes in an array) have no spelling. This ADR removes the four
gates. It is one design decision plus plumbing verification — the
layout story was settled in 0034.

## Decisions

1. **All four positions are legal:** parameters, value- and
   ref-struct fields, array element types, enum variant payloads. The
   diagnostics (`resolve_guarded`'s three messages and the
   arrays-of-error-unions arm in `resolve_type`) are deleted. Generic
   substitution composes with no new rules: `T := int!` produces
   these positions through the ADR 0035 machinery, so
   `Box<int!>`, `f<int!>(…)`, and `Result<int!, E>` come along for
   free.
2. **Unchanged bans stay:** `T!!`, `T?!`, `T!?` (the parser's
   mixing rejections — tag 1 stays reserved for `T?!`), `error!`
   redundancy, and `try` remains statement-positioned (its operand
   may now be any `T!`-typed expression, `try s.f` included).
   Builtin signatures are untouched (ADR 0032).
3. **The equality ban is recursive.** Direct `==` on `T!` was already
   rejected ("narrow first"); wrapping must not create a back door.
   A type is not eq-comparable when a `T!` is reachable through it by
   value: value-struct fields, enum payloads, optional inners. The
   walk cuts at refstructs and arrays — they compare by handle
   identity (ADR 0026) and never inspect contents, so `int![]` arrays
   and refstructs holding `T!` fields still compare as handles. Null
   tests (`x == null`, `x != null`) are exempt for the same reason:
   they read the optional tag, never the contents — and they are the
   narrowing idiom the ban exists to funnel users toward. The
   diagnostic names the reason (`contains an error union — narrow
   first`). Both engines' equality code never sees a reachable-`T!`
   aggregate, so neither needs an equality change.
4. **Narrowing reaches the new places by the existing machinery.**
   `place_vs_error` already records field-path facts; the consumption
   arm in `check_field` (the mirror of the optional one) makes
   `if s.f == error`/`!= error` read `s.f` as `error`/`T` in the
   proven region. Kill rules ride along unchanged — calls and field
   writes drop field facts, rebinding drops a path and its
   extensions. Indexed places (`xs[i]`) do not narrow, exactly like
   optionals: bind to a local first.
5. **No new layout, no new lowering shapes.** A `T!` field or element
   occupies `1 + words(T)` at its slot exactly as it does in a frame
   temp; params travel like any multi-word value (ADR 0024); copies
   happen at the oracle's copy points. Construction into the new
   positions reuses the same widening the flow positions use
   (value → tag 0 + payload, code → tag + zeroed payload). Printing
   recurses through the existing whole-union show routine.

## Consequences

**Positive:** error-carrying data structures become expressible in
the blessed channel; `Result<T, E>`-style and `T!`-style code now
compose with generics, enums, structs, and arrays symmetrically; the
0034 gates and their tests are deleted, not maintained.

**Accepted costs:** the eq-comparability check becomes a recursive
type walk (with the same cycle cut the layout walk uses); a struct
that adds a `T!` field silently loses `==` for its users — the
diagnostic names the field's union as the reason, and narrowing
before comparing remains the intended idiom.
