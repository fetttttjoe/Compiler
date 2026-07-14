# ADR 0020 — Divergence-Aware Narrowing

- **Status:** Accepted
- **Date:** 2026-07-14
- **Extends:** ADR 0007 (the narrowing rules), ADR 0019 (whose
  `break`/`continue` made the guard idiom the natural loop shape)

## Context

`if cur == null { continue; }` — the guard idiom — did not narrow: facts
appear only inside a narrowed block, and a reassignment inside any branch
kills its fact globally, even when that branch can never fall through.
With `break`/`continue` in the language this rejects the idiomatic way to
write loops (and `if p == null { return 0; }` at function tops). The
checker knows the branch diverges; it just didn't use that knowledge.

## Decisions

1. **Divergence is syntactic.** A statement list diverges when any of its
   statements is `return`/`break`/`continue`, or an `if` whose two
   branches both exist and both diverge. Loops never diverge (a contained
   `break` targets the loop itself; `while true` analysis stays out,
   consistent with definite-return).
2. **A diverging branch's narrowing side effects are rolled back.** Its
   writes cannot reach the statement after the `if` inside this
   iteration (the branch exits first), and cannot poison later
   iterations either: loop entry re-proves the loop condition's own
   facts, and enclosing facts a loop body can touch were already dropped
   before the body was checked (`body_effects` recurses into every
   branch, diverging or not). The rollback restores the fact stack
   captured before the branch; it never adds anything.
3. **The join adds only what the unique fall-through path implies.**
   After `if cond A else B`:
   - `A` diverges, no `else` → the condition's false-facts hold after
     (nothing existed to invalidate them).
   - exactly one branch diverges → the *surviving* facts of the
     fall-through branch's own frame hold after — surviving means still
     present at the branch's end, so writes inside it already subtracted
     themselves. `if p == null { return; } else { p = f(); }` adds
     nothing; `if p == null { return; } else { log(); }` adds `p`.
   - both diverge → nothing; code after is unreachable and stays
     conservatively unnarrowed (dead code is allowed, not analyzed).
4. **Function bodies get a base narrowing frame**, so top-level guard
   facts have a home and a top-level rebinding shadows them exactly like
   ADR 0007 demands.
5. **Loops gain no exit facts.** `while p == null { … }` proves nothing
   after the loop — `break` can exit with the condition still true.

## Consequences

**Positive:** the guard idiom narrows — early-return validation,
`continue`-skip loops, and `break`-out searches type without `else`
nesting; every rule adds facts only along the proven unique path, so the
compiled unchecked field loads (ADR 0007) stay sound.

**Accepted costs:** one fact-stack clone per diverging branch when facts
exist (guarded by the existing fast path); non-diverging branches keep
today's conservative global kills; `x != null` still yields no
false-facts, so inverted guards (`if x != null {} else { return; }`)
narrow only via branch survivors, not new negation machinery.
