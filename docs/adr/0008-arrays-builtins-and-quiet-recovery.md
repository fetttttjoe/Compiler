# ADR 0008 — Arrays, Builtins, and Quiet Recovery

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0006 (arrays adopt refstruct's reference semantics),
  ADR 0007 (optionals interact with element types and narrowing)

## Context

The language could compute but not communicate (no output) and could not
hold collections (no arrays) — the two gaps between "a type system demo"
and "a language someone can write a program in". Separately, every
expression error triggered a follow-on "expected return type X, found
unit" from the `Type::Unit` recovery value: one mistake, two or three
diagnostics.

## Decisions

1. **Poisoned recovery: `Type::Error`.** An expression that already
   produced a diagnostic types as `Error`, which fits everything and
   compares with everything — downstream checks stay silent. One mistake,
   one error. `Unit` remains a real type (unit-returning calls still
   type-check strictly); only *failed* expressions poison.
2. **Builtins are shadowable names, not keywords.** `print(v)`, `len(a)`,
   `push(a, v)` resolve only when no user definition has the name — the
   checker and interpreter share that resolution order, and the spellings
   live once in `syntax.rs` (`BUILTIN_*`). `print` accepts any single
   value (scalars/strings render raw; structs render debug-style —
   revisit with string interpolation).
3. **Arrays are reference types: `T[]`.** Growable, heap-shaped, aliased
   on assignment and calls, identity `==` — exactly refstruct's model
   (ADR 0006), so the language has one story for "value" (struct) vs
   "reference" (refstruct, arrays). Element writes (`a[i] = v`) go through
   the reference and are legal on `const` bindings; rebinding is not.
4. **Element typing is first-element inference, with two refinements.**
   *(Amended by ADR 0010: bindings now always declare their type, and a
   literal at a binding is verified element-by-element against the
   declaration — the inference below applies only in already-typed
   positions such as arguments and returns.)*
   `[1, 2, 3]` is `int[]`; mixed elements error ("must share one type");
   a leading `null` can't name a type (error + hint). A later `null`
   widens the element to optional (`[1, null]` is `int?[]`), and a later
   element pins down a leading empty (`[[], [1]]` is `int[][]`). `[]`
   types as a dedicated *unconstrained* element (`Type::Unknown` — not
   the poison type, which strictly means "diagnostic already emitted")
   that fits any array slot; a bare `var xs = [];` requires an annotation,
   exactly like bare `null`. Arrays are invariant (`int[]` never fits
   `int?[]` — an alias could push null).
5. **Index expressions are places but not narrowing paths.** `a[i].x = v`
   works (read-modify-write through the index); `a[i] != null` narrows
   nothing — element identity is dynamic. Bounds are checked at runtime
   ("index 5 out of bounds (length 3)"); negative indices are out of
   bounds by construction.
6. **Type suffixes compose left to right** — `int?[]` (array of
   optionals) vs `int[]?` (optional array), and `T??` gets a dedicated
   parse error ("nested optionals are not allowed — 'T??' is just 'T?'").

## Consequences

**Positive:** hello world exists; real programs (build a list, mutate,
aggregate, print) run end to end; diagnostics report one error per
mistake; the value-vs-reference split stays a two-word answer.

**Accepted costs:** no `for` loop yet (while + index covers it);
`len`/`push` are free functions rather than methods (no method syntax
exists); first-element inference means `[null, x]` needs reordering
(`[x, null]` widens fine).

**Deferred (named seats):** `for`, string interpolation, array slicing /
`pop` / removal, method-call syntax, `int?`'s tag cost hint.
