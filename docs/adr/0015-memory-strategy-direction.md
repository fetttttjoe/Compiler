# ADR 0015 — Memory Strategy Direction: Regions, Not Collectors

- **Status:** Proposed — direction pinned, mechanism deferred to its
  trigger
- **Date:** 2026-07-08
- **Extends:** ADR 0009 (which chose arena/leak initially and named
  ownership-or-RC as a later ADR), ADR 0012 (law 2)

## Context

ADR 0009's collector-free story — one arena, freed on exit — is correct
for short-lived programs and stays the status quo. But features drift
toward whatever memory model feels ambient, so the *direction* is pinned
now: which families of strategy are permanently out, and which one the
language is steering toward when long-running programs force the
question.

## Decisions

1. **Never a tracing GC.** Pause times, write barriers, and a runtime
   that walks the heap contradict the identity (ADR 0005) outright.
   This is a law, not a preference.
2. **Reference counting is rejected as the general strategy.** RC taxes
   the common case (a count update on every handle copy — arrays and
   refstructs copy handles constantly) and leaks reference cycles, which
   the language can already construct. It may reappear narrowly (e.g. a
   shared-ownership escape hatch) but never as the default.
3. **The direction is region memory:** allocations bind to a region;
   whole regions free at once, deterministically. Candidate shapes when
   the trigger fires — a function-scoped default region with escape
   inference, or explicit `arena { }` blocks — are evaluated in the
   mechanism ADR, against real programs. Inference, if chosen, may only
   *place* allocations, never change observable semantics ("nothing
   implicit" applies to lifetimes too).
4. **Until the trigger** (the first real long-running workload), the
   global arena freed on exit remains the model, and the interpreter's
   arena heap (ADR 0011) remains its faithful oracle.

## Consequences

**Positive:** deterministic costs, no pauses, no per-copy overhead;
every feature designed meanwhile can assume "allocations have a region"
without betting on a specific mechanism; the two dead ends (GC, default
RC) can no longer creep in implicitly.

**Accepted costs:** long-lived programs with mixed lifetimes are not
served until the mechanism ADR lands; that ADR inherits a real design
problem (escape inference vs. explicit blocks) rather than a solved one.
