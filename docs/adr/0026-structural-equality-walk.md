# ADR 0026 — Structural Equality: the Per-Field Walk

- **Status:** Accepted
- **Date:** 2026-07-15
- **Amends:** ADR 0021 (equality matrix), ADR 0013 (string equality)

## Context

`==` on value structs containing strings or floats was gated: one
memcmp cannot decide string content (descriptors differ, bytes agree)
or IEEE float equality (NaN ≠ NaN, +0.0 = -0.0). But the oracle's
semantics — Rust `PartialEq` over the value tree — decomposes into
legs the backend already has: `cmpeqsd` IS IEEE equality, and the
string-content sequence exists. The gate was never about semantics,
only about the single-memcmp shortcut.

## Decisions

1. **One comparator, `value_eq(type, a, aoff, b, boff)`**, replaces
   the three overlapping paths (`aggregate_eq`'s per-kind arms,
   `optional_eq`'s payload calls, `payload_eq`). Operands are always
   addresses — pointer plus byte offset; word kinds load, multi-word
   kinds take the interior pointer. It dispatches on the layout kind:
   - **Word:** one compare, `cmpeqsd` for floats (IEEE, matching the
     oracle's `f64::eq`), handle identity for refstructs/arrays.
   - **Str:** the existing content sequence (length, then memcmp).
   - **memcmp-able struct or optional:** one memcmp — canonical
     zeroed nulls (ADR 0021) keep tagged optionals in this class.
   - **no-memcmp struct:** a per-field walk in declaration order,
     short-circuiting on the first unequal field. Value structs
     cannot be recursive (infinite size), so the walk terminates
     statically — inline expansion, no generated routines. (Printing
     needed routines because refstruct *values* cycle at runtime;
     equality on refstruct fields is identity, no traversal.)
   - **no-memcmp optional:** tags equal, and both null or payloads
     equal (recursing at +8).
2. **Expression-level concerns stay where they were:** `optional_eq`
   keeps the null-literal tag tests and the `T? == T` mixed arms
   (present + payload compare, order-insensitive legs); `aggregate_eq`
   keeps the left-operand snapshot before the right evaluates. Both
   now delegate every value comparison to `value_eq`.
3. **The gate is gone.** `'==' on structs containing strings or
   floats' leaves the diagnostic inventory; memcmp-able types keep
   their one-memcmp fast path unchanged.

## Consequences

**Positive:** the equality matrix is total over compilable types;
float-field equality is IEEE-correct by construction; three
comparators become one (clearer SoC: expression concerns vs value
comparison); `not yet compilable` shrinks to float formatting.

**Accepted costs:** deep no-memcmp types inline a compare chain per
`==` site (bounded by type nesting, not data); memcmp-able tagged
optionals now compare via memcmp rather than inline tag/payload
compares — same result by canonicality, one call instead of a branch
pair.
