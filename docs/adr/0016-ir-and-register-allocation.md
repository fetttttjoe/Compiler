# ADR 0016 — The IR: Virtual Registers, Linear Scan, Tiered Rollout

- **Status:** Accepted — first tier landed. Measured (benches/, C -O2
  baseline): fib 4.7x→3.2x, collatz 5.5x→4.5x, primes ~1x, loop_sum
  3.6x→3.3x, mandel 6.6x→3.0x (XMM pool). Remaining gaps are optimizer
  passes (division strength reduction, hoisting) — the SSA follow-up.
- **Date:** 2026-07-08
- **Extends:** ADR 0009 (the "when a real optimization needs it" trigger),
  ADR 0012 (law 7 named the kit and this moment)

## Context

The benchmark suite puts the direct emitter at 3.5–6.6× off C -O2, and
the gap is one-shaped: every value round-trips through %rax and the
machine stack, and every local lives in a frame slot. That is register
allocation's job. The trigger ADR 0009 named has fired.

## Decisions

1. **A flat virtual-register IR, not SSA yet.** Instructions over
   unlimited vregs (const, copy, binop, call, branch, label, ret);
   locals are mutable vregs, so loops need no phi nodes. SSA is a
   *property* this IR gains later, when GVN/const-propagation (the
   passes that want it) land — pinned as the follow-up, not skipped.
2. **Linear-scan register allocation** over live intervals from
   iterative liveness. Two pools: callee-saved {rbx, r12–r15} for
   intervals crossing calls, caller-saved {r10, r11} otherwise; the
   pools exclude every argument register, so call setup can never
   clobber a live value. %rax stays the universal scratch; %rcx/%rdx
   stay reserved for division. Losers spill to frame slots.
3. **Tiered per function.** Lowering returns None on any construct the
   IR doesn't cover yet (aggregates, strings, builtins, sret), and that
   function compiles through the existing direct emitter. Both tiers
   share the ABI, so they interoperate freely in one binary. The IR's
   coverage grows slice by slice exactly like the backend itself did;
   the direct emitter retires when lowering stops returning None.
4. **First tier scope:** int/bool/float scalars, locals, all operators,
   if/while, direct calls between word-typed functions. That covers
   five of the seven benchmarks — the measurement of this ADR is its
   own before/after table.
5. **The differential contract is unchanged.** Same oracle, same
   harness, both tiers behind one `compile()`; a lowering bug is a diff
   failure like any other.

## Consequences

**Positive:** the measured waste (operand-stack traffic, memory-resident
locals) is exactly what vregs + linear scan remove; the tier boundary
means zero correctness risk for unlowered constructs.

**Accepted costs:** two emission paths exist until coverage completes —
bounded by the tier contract (shared ABI, shared tests); no GVN/DCE
until the SSA follow-up.
