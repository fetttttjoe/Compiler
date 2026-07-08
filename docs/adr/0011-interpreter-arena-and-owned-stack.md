# ADR 0011 — Interpreter Arena Heap and Owned Execution Stack

- **Status:** Accepted
- **Date:** 2026-07-08
- **Extends:** ADR 0009 (implements its collector-free arena story in the
  oracle), supersedes ADR 0006's `Rc<RefCell<_>>` plumbing and ADR 0007's
  accepted cycle leak

## Context

Two structural debts surfaced together. Runaway recursion crashed the
process (native stack overflow — the tree-walker recurses on whatever
stack its caller happens to have), and the first fix was a workaround: a
low depth cap sized to debug-build frames plus a `RUST_MIN_STACK` config
for test threads. Separately, `Rc<RefCell<_>>` handles made reference
cycles leak and kept `Value` from being `Send`, which is exactly what
blocked the real fix.

## Decisions

1. **Arena heap, typed.** Every refstruct object and array buffer lives
   in an interpreter-owned arena with a table per kind (`Heap { structs,
   arrays }`); `Value::Ref` indexes structs, `Value::Array` indexes
   arrays, so no handle/cell mismatch is even expressible. Nothing frees
   mid-run; the arena drops wholesale after execution — ADR 0009's memory
   story, so cycles are harmless by construction. Identity equality is
   index equality (derived `PartialEq`), and there are no borrow guards
   to hold across evaluation. Because nothing frees, runaway allocation
   is capped: exceeding `MAX_HEAP_CELLS` is a "heap limit exceeded"
   diagnostic, not an OOM kill.
2. **The interpreter owns its stack.** `interpret()` runs the program on
   a dedicated scoped thread with an explicit 256MB (lazily committed)
   stack — possible only because arena handles make `Value` `Send`. No
   caller environment, test harness, or rlimit shapes behavior.
3. **One depth budget for everything that recurses natively.** Calls,
   statements, and expression levels each charge one unit against
   `MAX_EVAL_DEPTH = 65_536`, sized so the budget binds long before the
   owned stack does even with fat debug frames (the arithmetic lives in
   the policy block). Charging calls alone was not enough — deep
   expressions inside deep recursion overflowed the stack anyway.
   Exceeding the budget is a diagnostic, never a crash.
4. **`interpret` returns `(Value, Heap)`** — rendering needs the arena,
   so the phase contract carries it. Output policy: a broken pipe means
   the consumer is done and terminates the program quietly (GNU
   convention — it must not keep computing into a dead pipe); any other
   write failure (full disk) is an error with a nonzero exit, never a
   silent success. Stderr writes are best-effort and never panic.

## Consequences

**Positive:** deep recursion is deterministic across debug/release/test
environments; the cycle leak is gone; `Send` values unblock any future
parallel or remote execution of the oracle; codegen inherits a heap model
that already matches its ADR.

**Accepted costs:** the arena never shrinks during a run (fine for the
oracle; real memory policy is codegen's, per ADR 0009); handles are
unchecked indices internally (never exposed to programs).
