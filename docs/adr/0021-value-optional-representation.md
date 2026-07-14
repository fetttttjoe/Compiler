# ADR 0021 — Value-Optional Representation: the Tag Word

- **Status:** Accepted
- **Date:** 2026-07-14
- **Extends:** ADR 0009 (which promised this layout), ADR 0017 (Phase C
  item), ADR 0012 (law 1: natural layout, no boxing)

## Context

`T?` compiles only for reference-shaped `T` (null = handle 0). For value
types the checker accepts what the backend refuses — the largest
checker-vs-backend gap. ADR 0009 already named the answer: a tag word.

## Decisions

1. **Layout: `{tag, payload}`, tag first.** `T?` of a value type `T`
   occupies `1 + words(T)` words: tag 0 = null, 1 = present, payload in
   `T`'s natural layout at +8. `int?`/`bool?`/`float?` are 2 words,
   `string?` 3, a value-struct `?` is `n+1`. Reference-shaped `T?` stays
   a nullable handle (free). Optionals never nest (parser-rejected).
2. **Null is canonical: the payload words are zeroed** at every null
   construction (`null` wrap, missed `?.`). Copies preserve canon, so a
   struct embedding `int?`/`bool?` fields stays memcmp-comparable;
   `float?`/`string?` payloads set `no_memcmp` (IEEE, content equality).
3. **Storage shape follows the declared type; narrowing unwraps reads.**
   A binding or field declared `T?` is Opt-shaped storage (a pointer to
   its words, like every multi-word value). Where the checker recorded
   the *inner* type for a use (narrowing proved presence), the read goes
   through the tag: payload load at +8 (word) or interior pointer
   (multi-word). Recorded `T?` reads stay whole-value pointers.
4. **Wrap points mirror the fits rule.** Wherever `T` or `null` flows
   into a `T?` slot — let, assignment, call argument, return, struct
   literal field, field assignment — lowering wraps: tag 1 + payload
   copy, or the canonical zeroed null. Arrays of value optionals stay
   gated (ADR 0014's stride seat; a pushed word would alias null).
5. **Operators.** `x == null` is a tag test. `T? == T?` compares
   presence then payload; `T? == T` requires presence; payload equality
   follows the payload's class (words, IEEE floats, string content,
   struct memcmp — `no_memcmp` payloads keep the existing struct-equality
   gate). The left operand snapshots first, as everywhere. `a ?? b`
   selects on the tag: rhs fitting the inner type unwraps the result;
   optional/null rhs keeps the optional shape. `p?.x` now builds value
   optionals: null base → canonical null; present → wrap the field (or
   copy it whole when the field is already optional — flattening).
6. **`print` accepts `int?`/`bool?`/`string?`** — payload rendering or
   the literal `null`, exactly the oracle's text. `float?` printing
   stays gated with float printing itself; aggregate payloads stay
   gated with aggregate printing.
7. **The interpreter and checker are untouched.** `Value::Null` was
   always the normative semantics; the typing rules existed. This ADR
   is layout plus lowering — the gate lived in `kind_of` alone; the
   feature ships only when every lowering site behind it exists.

## Consequences

**Positive:** the checker/backend gap closes for every optional except
optional-element arrays; guard-narrowed code (ADR 0020) lowers to
unchecked payload loads; no boxing, no hidden allocation — the cost is
one visible word, exactly as ADR 0009 documented.

**Accepted costs:** value optionals copy `1 + words(T)` words at the
oracle's copy points; the equality matrix grows a payload-class
dispatch; `int?[]` remains a named gap until the stride ADR.
