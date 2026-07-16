# ADR 0036 — Payload Enums and `match`

- **Status:** Accepted
- **Date:** 2026-07-16
- **Extends:** 0021/0034 (the tag-word layout generalizes), 0035
  (templates and the monomorphizer carry enums), 0017 Phase D (sum
  types)

## Context

Error unions gave failure a value but deliberately stopped short of
general sum types (ADR 0034). With generics landed, the blocker is
gone: `Result<T, E>`-shaped code needs a way to declare "one of these
variants, each with its own payload" and a way to branch on which one
is live. TS has no payload enums; the maintainer chose the Rust shape
over discriminated unions — a self-contained declaration beats a
convention over structs.

## Decisions

1. **Declaration.** `enum Shape { Circle(float), Square(float, float),
   Ready }` — variants carry zero or more positional payload types.
   Generic from day one: `enum Result<T, E> { Ok(T), Err(E) }`
   instantiates through the ADR 0035 template machinery, canonical
   names and all. Enums are value types: they copy like structs and
   never alias. `enum` and `match` become keywords (the ADR 0034
   reservation cost, paid twice more).
2. **Construction is qualified:** `Shape.Circle(1.5)`,
   `Result<int, string>.Ok(3)` — mirroring `error.Name`; variant names
   never pollute the value namespace. Nullary variants still call:
   `Shape.Ready()` — nothing implicit, construction is always an
   expression with parentheses. The checker records each construction
   site span-keyed (like `error_lits`); engines never resolve a
   variant name.
3. **Consumption is `match`, a statement, arms are blocks:**

   ```
   match s {
       Circle(r) { print(r); }
       Square(w, h) { print(w * h); }
       else { print("none"); }
   }
   ```

   No `=>` token (arms read like `if` bodies), no commas, no nested
   patterns — an arm is a variant name, optional payload bindings
   (const, one per payload, `_` skips one), and a block. `else` covers
   the rest. **Exhaustiveness is checked:** every variant appears at
   most once, and the arms plus `else` must cover all of them. An
   `else` on an already-exhaustive match stays legal — the divergence
   and definite-return analyses are syntax-only (they never see
   types), so `else` is also how a fully-returning match proves it
   never falls through.
4. **Match joins the flow analyses** (the framework investment): a
   `match` with an `else` whose arms all diverge diverges, and
   definite-return (`always_returns`) counts it — both remain pure
   syntax, hence the `else` requirement above. Payload bindings scope to their arm and thread
   through narrowing as ordinary consts.
5. **Layout: one tag word + max-payload words,** the ADR 0021/0034
   shape generalized. Tags are 0-based in declaration order. Unused
   payload words zero at construction (canonical, like the optional
   null), so equality memcmps whenever every payload is
   memcmp-safe; otherwise it compares tags, then walks the live
   variant's payloads (structural, ADR 0026's doctrine). Enums print
   as `Circle(1.5)` / `Ready` — the interpreter's render is normative,
   the compiled show routines reproduce it byte for byte.
6. **Interoperation, not replacement:** `T!` stays the blessed
   failure channel with `try`; `Result<T, E>` is now expressible for
   code that wants errors as ordinary data. Neither wraps the other
   implicitly (ADR 0005).
7. **Out of scope, deliberately:** match-as-expression (ys has no
   block expressions), guards, nested patterns, variant methods,
   ref-semantics enums (wrap a refstruct payload instead), implicit
   variant imports. Each returns with its own ADR when need pulls.

## Memory and lowering story

An enum value is `1 + max(variant payload words)` words in a frame
temp or field slot, traveling by pointer like any multi-word value —
copies at exactly the oracle's copy points. Construction stores the
tag, the payloads, and zeroes the slack. `match` lowers as a tag
compare chain into arm blocks (the `err_chain` shape); payload
bindings copy out of the scrutinee before the arm body runs, so
mutation inside the arm can't alias the matched value. Generic enums
monomorphize; instances have exactly the layout the hand-written
enum would have.

## Consequences

**Positive:** `Result<T, E>`, `Option`-likes beyond `T?`, state
machines, and AST-shaped user code become expressible with
exhaustiveness the compiler enforces. The tag-word idiom now has one
general form instead of two special cases.

**Accepted costs:** two more reserved words; enums sized by their
largest variant (inherent to unboxed sums, ADR 0012's trade);
`match` is the first statement whose header binds names — the parser
and every flow analysis learn one new shape.
