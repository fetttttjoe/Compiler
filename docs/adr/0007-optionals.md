# ADR 0007 — Optionals: `null`, `T?`, `?.`, `??`, Narrowing

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0006 (gives refstructs their base case), ADR 0005 (the
  hint diagnostic pattern gets its first instance)
- **Spec:** docs/superpowers/specs/2026-07-07-refstruct-and-optionals-design.md

## Context

Reference types without a null story can't express self-reference — no
lists, no trees. And any null story done implicitly (JS `undefined`,
everything-nullable) contradicts ADR 0005. Optionality had to be spelled in
the type.

## Decisions

1. **One nothing-value: `null`.** Never `undefined` — one concept, the
   systems term, and `Node?` compiles to a nullable pointer for free.
2. **Postfix `?` type: `T?` = "T or null".** Any type can be optional;
   `int?` will cost a tag in codegen (documented, future hint), `Node?`
   costs nothing. Storing a `T` (or `null`) where `T?` is expected
   satisfies the type — `fits()` in `check.rs` is the one compatibility
   rule, used at every boundary (init, assign, args, returns, fields).
3. **`null` is context-typed.** The literal has its own internal type
   (`Type::Null`) that fits only `T?` slots; a bare `var x = null;` is an
   error with the annotation hint. Bindings gained optional annotations
   (`var head: Node? = null;`) for exactly this.
4. **Plain `.` on an optional is an error with a hint** — "may be null —
   use '?.', or check '!= null' first". The first diagnostic of ADR 0005's
   hint class. `?.` on a never-null type is equally an error ("use '.'").
5. **`?.` short-circuits to null**; its result is `U?` (already-optional
   fields stay flat — no `T??` exists anywhere). `?.` links are not
   places: you can't assign through one.
6. **`??` coalescing** is the unwrap: `a ?? b` takes `a` unless null, else
   the lazily-evaluated `b`. Lowest precedence; left side must be optional;
   the result unwraps when `b: T`, stays optional when `b: T?`/null.
7. **Narrowing:** `x != null` narrows `x` in `if`/`while` bodies,
   `x == null` narrows the `else` branch, a leading `x != null &&`
   narrows the rest of the condition. Rebinding or shadowing un-narrows
   (typed before the rebind, so `cur = cur.next` inside the loop works).
   No cross-function inference. Documented limit, not an accident.

   > **Amendment (2026-07-07):** narrowing extended from bare names to
   > place paths — `while cur.left != null { cur = cur.left; }` now
   > checks. Soundness under aliasing is kept by invalidation, not
   > tracking: a field-path fact dies on any call, any write through any
   > field, or rebinding of any prefix; bare-variable facts survive calls
   > (a callee cannot rebind the caller's locals). Invalidation is
   > position-aware: a call in the right side of `&&` kills the left
   > side's field facts before they reach the body; entering a `while`
   > body drops enclosing facts the body can invalidate (they would be
   > stale on iteration 2 — the loop's own condition re-checks, outer
   > guards don't); shadowing only hides a fact while the shadow's scope
   > lives, while reassignment kills it permanently. This is stricter
   > than TypeScript (which keeps property narrowing across calls,
   > unsoundly) and preserves the invariant that checked programs cannot
   > hit runtime null errors.

## Consequences

**Positive:** linked structures now exist end to end (build, traverse,
mutate — the interpreter test suite closes with exactly that program);
null-safety is enforced at compile time with actionable hints; every
compatibility check routes through one `fits()` function that codegen can
reuse.

**Accepted costs:** narrowing is conservative — patterns it can't see
require `??` or restructuring; `int?`'s future tag cost is invisible until
the codegen hint lands.

**Revisited from ADR 0006:** reference cycles are now constructible
(`a.next = a`). ~~The interpreter's `Rc` can leak~~ — resolved by ADR
0011's arena heap, where cycles are harmless by construction.
