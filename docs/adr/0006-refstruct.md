# ADR 0006 — `refstruct`: Explicit Reference Types

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0005 (which reserved explicit references as their own ADR)
- **Spec:** docs/superpowers/specs/2026-07-07-refstruct-and-optionals-design.md

## Context

Value structs (ADR 0005) copy on every assignment and call, so no function
can mutate its caller's data. The language needed a reference type that is
opt-in, visible at the declaration, and honest about its costs — not
TypeScript's silent everything-is-a-reference.

## Decisions

1. **Declaration-site refness.** `refstruct Node { x: int }` — the type is
   by-reference everywhere, always. Use-site `ref T` annotations were
   rejected: they double the type grammar and make "does this call alias?"
   a per-site question instead of a per-type fact.
2. **Reference semantics.** A refstruct literal allocates one shared
   object; assignment, argument passing, and returns copy the handle.
   Mutation through any alias is visible through all.
3. **`const` forbids rebinding, not mutation through the reference** —
   C's const-pointer-to-mutable-data model. Since params are const
   bindings, `fun bump(c: Counter) { c.n = c.n + 1; }` works on the
   caller's object.
4. **Assignment mutability rule.** `a.b.c = v` needs a `var` root unless
   the place chain crosses a refstruct boundary — past a reference the
   write hits the shared object, not the binding. Replacing which object a
   field points to (`b.r = other`) still mutates the holder and needs
   `var`.
5. **Equality is identity.** `==` on refstructs is "same object"
   (`Rc::ptr_eq`) — what TS `===` and C pointer compare both do, O(1),
   nothing hidden. Value-struct equality recursing into a refstruct field
   compares that field by identity too. Deep comparison stays explicit:
   compare fields.
6. **Plumbing.** `by_ref` on `ast::Struct`/`check::StructType`;
   `Resolutions.ref_structs` (per-module visible refstruct names) tells the
   interpreter — and later codegen — which literals allocate. The
   interpreter models handles as arena indices (`Rc<RefCell<_>>` originally;
   superseded by ADR 0011) with identity equality; place assignment is
   read-modify-write (value hops clone, ref hops mutate the heap object).

## Consequences

**Positive:** functions can finally mutate caller data, opted into per
type; aliasing is a per-type fact readable at the declaration; codegen maps
this directly to heap pointer vs. stack value.

**Accepted costs:** interpreter value hops clone intermediates (fine for
the oracle); a same-module `struct P` and `refstruct P` share one
namespace (collision is an error, as before); `Debug` printing of refs is
raw.

**Deferred:** optionals (`null`, `T?`, `?.`, `??`, narrowing) — Phase 2 of
the spec; without them self-referential refstructs are declarable but
unconstructible, so reference cycles cannot exist yet and `Rc` cannot leak.
Cycle policy must be revisited when optionals land.
