# ADR 0024 — Stack Argument Passing: the Outgoing-Args Area

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0016 (frame layout), ADR 0018 (one backend)

## Context

Both `> 6` gates (function definitions and call sites) exist only
because arguments beyond the six SysV registers need stack slots, and
the backend's alignment story is "never push operands, so %rsp stays
16-aligned at every call with no fix-ups". Naive pushes would break
that invariant and the frame comment in emit.rs. The checker and
interpreter never had an arity limit — this is layout only.

## Decisions

1. **Argument slots, sret included.** A call's slot `i` (0-based; a
   struct-returning callee's hidden destination pointer occupies slot
   0) lands in `ARG_REGS[i]` for `i < 6` and at `8*(i-6)(%rsp)` for
   `i >= 6`. The callee reads slot `i >= 6` at `16 + 8*(i-6)(%rbp)` —
   above its saved %rbp and return address.
2. **A reserved outgoing-args area, not pushes.** The frame grows to
   `[saved callee regs | spills | temps | outgoing args]`, the
   outgoing area at the bottom (adjacent to %rsp), sized as the
   maximum stack-slot count over the function's calls and folded into
   the one prologue `subq`. %rsp never moves after the prologue; the
   alignment invariant survives verbatim. Runtime calls (`CallRt`)
   never exceed three arguments and are untouched.
3. **Every slot stays one word.** Multi-word values already travel as
   snapshot pointers (the call-argument copy discipline), so arity is
   independent of value width; floats keep the internal all-GPR
   convention (bitwise `movq`), stack slots included. No SysV XMM
   classification — these calls are internal-only.
4. **Stores happen at the call, evaluation stays where it was.**
   Arguments evaluate left to right into vregs exactly as today; the
   register moves and stack stores form one block immediately before
   `call`, so nested calls (which complete during evaluation) can
   reuse the same outgoing area without clobbering.

## Consequences

**Positive:** both gates lift — any arity compiles; parameter intervals
start at instruction 0 (no defining instruction, so liveness reaches
entry), meaning stack-slot params load once in the prologue and behave
like every other vreg thereafter.

**Accepted costs:** one extra load per stack argument and per stack
parameter; frames of functions making wide calls grow by the max
outgoing slot count, even on paths that never make the wide call.
