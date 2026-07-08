# ADR 0012 — Performance Laws and the Road to C Economics

- **Status:** Accepted
- **Date:** 2026-07-08
- **Extends:** ADR 0005 (the identity these laws defend), ADR 0009 (the
  backend direction they schedule)

## Context

The identity is TypeScript surface with C economics. The decisions that
determine whether a language *can* reach C-grade speed are semantic —
representation, aliasing, dispatch — and retrofitting them after features
ship is the expensive failure mode. The backend now exists (first slice),
so the laws that keep every future feature compilable to fast code are
pinned here, before those features do.

## Decisions

1. **No uniform or boxed representation, ever.** Every type compiles to
   its natural machine layout. When generics arrive they monomorphize —
   one specialization per instantiated type, like C++/Rust, never a
   boxed-pointer runtime representation. This is "nothing implicit"
   (ADR 0005) applied to memory.
2. **Layout is part of a feature's design.** Every feature ADR states its
   memory layout and lowering story (re-affirming ADR 0009). A feature
   whose layout story is "the backend will figure it out" is not designed.
3. **Aliasing guarantees are API.** Value types never alias; reference
   types alias only within their own type. No future feature may weaken
   this — the optimizer's precision (and the auto-vectorization seat
   below) depends on guarantees C compilers have to approximate.
4. **The static call graph is the default.** Every call today resolves at
   check time. Function values and dynamic dispatch, when added, are
   explicit opt-in syntax; direct calls stay the norm so inlining stays
   complete and cheap.
5. **Benchmarks land with loops.** As soon as codegen runs loops, a
   `benches/` suite compares compiled output against equivalent C at
   `-O2`. Performance claims get numbers; the hint diagnostics (ADR 0005)
   cite measured costs, not folklore.
6. **Differential fuzzing lands with the IR.** A random-program generator
   feeding both engines joins the suite when optimization begins —
   optimizers are where wrong-code bugs live, and the oracle architecture
   is purpose-built to catch them.
7. **The IR trigger, re-pinned.** Direct AST→assembly continues through
   locals, control flow, and calls. Then one small SSA IR carries the
   known kit — mem2reg/SROA (value structs dissolve into registers),
   inlining over the static call graph, linear-scan register allocation,
   light GVN/DCE/const-folding. That kit, on this language's shape, is
   the road to within a small factor of C. No other IR forms exist.
8. **Named seats, each waiting on its trigger, each its own ADR:**
   sized integers (`i32`, `u8`, …) for memory density; a structure-of-
   arrays layout modifier (`soa T[]`); SIMD — auto-vectorized `for-in`
   first (legal by law 3), explicit vector types later; compile-time
   execution reusing the interpreter as the comptime engine; structured
   concurrency built on the value/ref split (values are trivially
   sendable; refstructs need a transfer rule).

## Consequences

**Positive:** every future feature pays a small design tax (its layout
story) and in exchange the backend never inherits an uncompilable
semantic; "as fast as C" becomes a benchmarked claim, not a slogan.

**Accepted costs:** monomorphization trades binary size for speed
(documented, measurable); rejecting boxed generics closes off some
dynamic patterns TypeScript users may expect — the identity tiebreaker
(explicit beats implicit) already decides those arguments.
