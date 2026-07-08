# ADR 0017 — Settling the Language: Executable Spec and the Road Map

- **Status:** Accepted
- **Date:** 2026-07-08
- **Extends:** every prior ADR — this one arranges them into a stable
  platform for what comes next

## Context

The language now has full surface coverage in two engines and a backend
within ~2.4× of C. Before features pile on, the language itself needs
settling: what defines it, what may still change, and in what order the
open work lands. Standard practice is a prose specification plus a
conformance suite plus a stability policy (the Go spec, the Rust
reference, test262). Prose specs drift from implementations; this
project has an asset most languages lack, so the settlement leans on it.

## Decisions

1. **The semantics are executable: the interpreter is the normative
   spec.** Where a question of behavior arises, the interpreter's answer
   is the language's answer (crashes and accepted bugs excepted — those
   get fixed *as spec errata*, with an ADR note). Prose exists only for
   the invariants code can't show: the laws in ADR 0005/0012, layout in
   0013/0014, and the render-format contract (name-sorted fields,
   Rust-Display scalars). This inverts the standard: instead of testing
   the implementation against a document, implementations are tested
   against the reference implementation.
2. **The grammar has one source of truth** — the parser, with
   `tools/gen-grammar.py` deriving the human-readable grammar. A grammar
   change is a parser change; the derived document is regenerated, never
   hand-edited.
3. **A conformance corpus, decoupled from both engines.** `conformance/`
   holds `.ys` programs with golden outputs (`.out`: the oracle's exact
   stdout — print lines plus the result line). Every engine, present and
   future (the compiled tier today, a self-hosted compiler someday),
   must reproduce the goldens: stdout byte-for-byte, exit code = result
   masked to 8 bits. The corpus is the portable definition of the
   language; tests/conformance.rs enforces it for both current engines
   on every run. New features land with corpus files in the same commit.
4. **Stability policy (pre-1.0):** behavior changes are allowed but
   never silent — a change that alters any golden must update the corpus
   in the same commit and say so in an ADR. When the Phase C list below
   empties, the corpus freezes additive-only and the language calls
   itself 1.0.
5. **The road map, in dependency order:**
   - **Phase B — backend maturity:** optimization passes on the ADR 0016
     tier (strength reduction now; SSA, GVN, inlining next); tier
     coverage growth (arrays, aggregates); runtime-error parity
     (compiled traps become messages + exit 1, closing the SIGFPE/abort
     divergences).
   - **Phase C — language completion,** each with ADR + corpus files:
     int↔float conversion (the mandel benchmark already hit this gap),
     `break`/`continue`, value-optional representation (the tag word ADR
     0009 promised), string interpolation and building (ADR 0013's named
     seat), `main` argument access, struct/array printing (render
     contract above).
   - **Phase D — the big rocks,** one ADR each, in whatever order need
     dictates: error handling (none exists — the largest unsettled
     design), generics (monomorphized, law since ADR 0012), the memory
     mechanism (ADR 0015's trigger), comptime via the interpreter,
     concurrency on the value/ref split, SoA/SIMD.
6. **A name is still owed.** The language is "ys" by file extension and
   habit; naming is the maintainer's call and blocks nothing, but 1.0
   should not ship under a file extension.

## Consequences

**Positive:** any future session, contributor, or reimplementation gets
truth from three artifacts — the interpreter (semantics), the generated
grammar (syntax), the corpus (conformance) — none of which can drift
from reality, because reality is what they are. The road map means no
feature lands ahead of its dependencies.

**Accepted costs:** the interpreter carries spec-grade responsibility
(its bugs are spec bugs until errata'd); prose stays deliberately thin,
which asks readers to run programs to answer edge questions — by design.
