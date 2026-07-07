# ADR 0005 — Language Identity: TS Surface, C Semantics

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0001 (confirms the reserved codegen seat's direction),
  ADR 0002 (settles struct semantics the moment they became observable)

## Context

Struct values now exist at runtime, which forced the first real semantics
question: when a struct is assigned or passed, does the receiver get the
value or a reference to it? TypeScript (the surface inspiration) says
reference; C says value. The answer shapes everything downstream — the
mental model users learn, and the entire backend (memory layout, calling
convention, whether a GC is ever needed). Decided now, before more code
observes either behavior.

## Decisions

1. **TypeScript surface, C semantics.** The syntax stays TS-flavored
   (`import { } from`, `const`/`var`, annotated signatures), but the
   execution model is C's: what you write is what runs, with no implicit
   machinery underneath. This pairing *is* the language's identity.
2. **Structs are value types.** Assignment, argument passing, and returns
   copy the value. Mutating a copy never affects the original —
   `fun grow(p: Point)` cannot change the caller's `p`. This is documented
   behavior the user is expected to know, not a gap to be papered over.
   References, if they ever exist, will be explicit syntax and their own
   ADR — never an implicit default.
3. **Nothing implicit, as a law.** Already true case by case (no int↔float
   coercion, no truthiness); this ADR promotes it from habit to rule: any
   future feature that would insert hidden conversions, hidden allocation,
   or hidden control flow is rejected by default.
4. **The compile target is native assembly.** The long-run product is
   ahead-of-time compiled machine code for systems, efficiency first — the
   `codegen(ast, resolutions) -> Vec<u8>` seat from ADR 0001 is confirmed,
   not revisited. Value semantics feeds it directly: structs get stack
   allocation and predictable layout; nothing in the language yet requires
   a heap or runtime support.
5. **Optimization hints are diagnostics.** The existing diagnostic
   machinery (severities, help text, suggestions) gains a hint class once
   codegen can back it with facts — the compiler telling the user "this
   copy is large, this call can't inline" the same way it reports type
   errors today. Named seat; not built before the backend exists.

## Consequences

**Positive:** one sentence explains the language ("TypeScript syntax, C
semantics, typed by default"); every semantics question downstream has a
tiebreaker (explicit beats implicit); the backend can assume value
semantics — stack allocation, copyable layouts, no GC.

**Accepted costs:** TS users will be surprised that mutating a struct
parameter doesn't propagate — that's a documentation duty, not a design
bug; large structs copy on every call until explicit references or a
codegen hint ("consider passing by reference") exist; interpreter clone
cost is accepted (it's the oracle, not the product).

**Deferred (named seats):** explicit references (own ADR, if ever), the
codegen backend ADR (ISA choice, textual asm vs. direct emission, x86-64
Linux first is the working assumption), the hint diagnostic class.
