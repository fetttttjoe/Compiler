# ADR 0022 — Runtime-Error Parity: Traps Become Diagnostics

- **Status:** Accepted
- **Date:** 2026-07-14
- **Extends:** ADR 0008 (whose bounds check this upgrades), ADR 0017
  (Phase B named this), ADR 0018 (deferred-trap policy, now closed)

## Context

The interpreter diagnoses division by zero, `i64::MIN / -1`, and
out-of-bounds indexing with a rendered error and exit 1. Compiled
programs hit the same states as SIGFPE and SIGABRT — the documented
deferred-trap policy. For users, a signal with no message is the worst
of the class; the divergence was scheduled to close, and the pieces
(bounds checks, span-carrying lowering, a SourceMap) all exist.

## Decisions

1. **Compiled runtime errors print and exit 1.** On stderr:
   `error: <message>` and ` --> file:line:col` — the first two lines of
   the oracle's diagnostic, message text identical ("division by zero",
   "division overflow", "index %ld out of bounds (length %ld)"). The
   source-excerpt lines stay interpreter-only: a binary does not carry
   program text.
2. **Locations resolve at compile time.** Lowering resolves each trap
   site's span through the SourceMap and interns one NUL-terminated
   location string per site (deduplicated, .rodata). The SourceMap
   threads into `codegen::compile`/`dump_ir` and the Lowerer.
3. **Division checks live on the general path only.** `Div`/`Rem` with
   a runtime divisor lower to a checked form: divisor-zero and
   MIN/-1 tests branching to trap stubs before `idiv`. Strength-reduced
   divisions (pow2/magic, constant divisor ≥ 2) cannot trap and stay
   unchecked — the benchmark-relevant paths are untouched.
4. **Bounds checks reroute.** The existing ADR 0008 check calls
   `ys_trap_oob(index, length, loc)` instead of `abort`.
5. **Trap stubs are runtime routines** (`ys_trap_div0`,
   `ys_trap_overflow`, `ys_trap_oob`): `dprintf(2, …)` then `exit(1)`.
   All new symbols join the codegen symbol inventory — named once,
   never re-spelled (the no-hardcoded-strings rule).
6. **Accepted remaining signals**, documented not closed: allocation
   exhaustion (malloc/realloc returning null) and native stack overflow
   on deep recursion. The interpreter's heap and depth budgets are
   interpreter policy (ADR 0011), not language semantics; closing these
   needs its own trigger and ADR.
7. **The differential contract widens for error programs** informally:
   CLI tests assert both engines exit 1 with the same core message.
   The clean-program contract (stdout + exit code) and every golden are
   untouched. The old SIGABRT/SIGFPE expectations flip — that was the
   gap, not the contract.

## Consequences

**Positive:** compiled failures become actionable (message + location);
the last user-visible behavior-class divergence between the engines
closes; `cc`-linked binaries still contain no unwinding or handler
machinery — two compares and a dead branch per runtime division.

**Accepted costs:** general-path divisions grow two predictable
branches (benches re-measured in this change); binaries carry one small
location string per trap site; stderr text beyond the first two lines
still differs from the interpreter (excerpt, colors).
