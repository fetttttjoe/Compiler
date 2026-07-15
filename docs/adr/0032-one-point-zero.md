# ADR 0032 — 1.0: the Corpus Freezes and the Language Is Named

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** 0017 (resolves decisions 4 and 6)

## Context

Phase C of ADR 0017 is empty: every named seat shipped — loop control
(0019), value optionals (0021), aggregate printing (0025), int↔float
conversion (0028), string conversion and building (0029), template
literals (0030), and the world interface (0031). Per decision 4, an
empty Phase C triggers the corpus freeze and the 1.0 declaration.

The remaining soft spots were audited before declaring: guard-return
narrowing on locals, whole-file read conveniences, multi-line template
text, and the fuzzer's coverage gaps are all additive — none can move
an existing golden. The goldens pin stdout bytes and exit codes only;
stderr diagnostics remain evolvable, which is exactly the room Phase D
error handling needs.

## Decisions

1. **The language is 1.0.** The conformance corpus freezes
   additive-only: new programs and goldens may land, existing goldens
   are immutable. The sole escape hatch is the one ADR 0017 decision 1
   already grants — an interpreter bug fixed as spec errata, with its
   own ADR naming the golden it moves. What was pre-1.0 policy
   ("never silent") is now a compatibility break.
2. **The name is ys.** The file extension is promoted to the official
   name (decision 6 resolved). No rename, no rebrand — the language
   ships as what it has been called all along.

## Consequences

**Positive:** downstream users, future engines, and a self-hosted
compiler get a byte-stable contract — any program in the corpus runs
identically forever. Phase D work (error handling first) proceeds
against a fixed surface: failure-as-value plus new goldens, never
edits to old ones.

**Accepted costs:** existing stdout formats — render lines,
float printing (0027), aggregate printing (0025) — are permanent,
quirks included. A better idea now costs an erratum ADR and a
compatibility note rather than a quiet golden update.
