# ADR 0033 — Narrowing Facts Reach Locals

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0007 (the narrowing rules), ADR 0020 (whose guard
  idiom this completes)

## Context

`if x == null { return; }` narrows `x` afterwards when `x` is a
parameter — and silently fails to when `x` is a local. The root cause
is in the fact stack: `bind` inserts a new binding's name into the
innermost frame's `shadowed` set so that *outer* facts about a
same-named *outer* binding stop applying. But `is_nonnull` consults
`shadowed` before `facts` within each frame, so a fact established
*after* the binding — necessarily about the new binding itself — is
hidden by its own shadow. For a top-level `var`/`const` the shadow
lands in the function's base frame, which lives until the function
ends: no later guard on that local can ever narrow it. Parameters
enter scope without `bind`, which is the whole asymmetry.

ADR 0031 made bind-guard-use the daily shape (`open`, then guard,
then use), and ADR 0034's error unions make it the canonical one —
the gap graduated from annoyance to blocker.

## Decisions

1. **Within a frame, facts outrank shadows.** `is_nonnull` checks
   `facts` before `shadowed` in each frame (outer frames unchanged:
   an inner shadow still hides outer facts). Sound because a fact can
   only enter a frame after the frame's shadow exists, and conditions
   test the visible binding — the shadowing one.
2. **`bind` kills same-frame facts covering the name** before adding
   the shadow. Rebinding still invalidates stale facts — the job the
   shadow-before-fact order was accidentally doing.

## Consequences

**Positive:** guard-return narrowing works uniformly on parameters and
locals; the resource idiom needs no `else` nesting; checker-only, and
the checker accepts strictly more programs — no golden moves, additive
per ADR 0032.

**Accepted costs:** none observed — same data structures, a check
reorder plus one removal loop in `bind`.
