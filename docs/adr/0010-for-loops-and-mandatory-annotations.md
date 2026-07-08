# ADR 0010 — `for` Loops and Mandatory Binding Annotations

- **Status:** Accepted
- **Date:** 2026-07-08
- **Extends:** ADR 0008 (supersedes the inference half of its decision 4),
  ADR 0005 (explicitness promoted one step further)

## Context

Two user decisions. First, iterating arrays with `var i` + `while` was the
last piece of boilerplate in every array program. Second, the array-literal
inference rules (first-element typing, null-widening) were clever machinery
deciding types the programmer never wrote — against the language's
explicit-over-implicit identity. The call: the programmer always declares
the type; the compiler verifies, it does not guess.

## Decisions

1. **`for x in xs { … }`** iterates an array; `x` is a `const` binding of
   the element type, fresh each iteration. **`for [i, x] in xs`** also
   binds `i` as a `const int` index — opt-in, so the common case stays
   noise-free; the two names must be distinct. The element type flows from the
   iterable (chain-derived types need no annotation). Iteration is live —
   length and elements re-read each step; the element is copied out before
   the body runs. Loop bodies get the same fact-invalidation rule as
   `while` (enclosing narrowing facts the body can invalidate are dropped
   on entry), and the loop variable shadows same-named narrowing facts.
2. **Every `var`/`const` declares its type.** `var x = 5;` is an error
   ("missing type annotation" + hint). Parameters, returns, and fields
   were already annotated; loop variables are chain-typed by design.
3. **Literals are verified against declarations, never inferred into
   them.** Wherever a type is declared — bindings, reassignments,
   arguments, returns, struct fields — an array literal is checked
   element-by-element against the declared element type, recursing
   through nested literals and optional wrappers: `var xs: int?[] =
   [1, 2];`, `xs = [3, 4];`, `g([5, 6])`, and `int?[][] = [[1]]` are all
   legal. The internal first-element inference (with null-widening)
   survives only where nothing is declared: loop iterables and operands.
   Iterating a fully unconstrained literal (`for x in [[]]`) is an error —
   a binding can never carry an unconstrained type.
4. **Mixed-type arrays wait for union types** — deliberately deferred to
   their own ADR; a union is a type-system feature (fits, narrowing,
   equality, codegen layout), not an array feature.

## Consequences

**Positive:** every binding's type is readable at the declaration; array
literals never surprise (`int?[]` holding ints is exactly what it says);
one blanket rule replaces three inference special cases in the mental
model; `for` removes the index-loop boilerplate from every aggregation.

**Accepted costs:** more keystrokes per binding than TypeScript;
`[1, "a"]` errors element-wise against the declared type rather than
being expressible (until unions).

**Deferred (named seats):** union types, ranges
(`0..n`), `break`/`continue`.
