# ADR 0023 — Multi-Word Array Elements: Compile-Time Stride

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0014 (which named this seat), ADR 0012 (law 1:
  natural layout, no boxing)
- **Amends:** ADR 0021 (lifts the `int?[]` gate)

## Context

ADR 0014's buffer stores single words; `kind_of`'s Array arm gates
everything wider. `string[]`, value-struct arrays, and `int?[]` type-
check and interpret but refuse to compile — the largest remaining
checker/backend gap. The element type is static at every site
(monomorphic language; future generics monomorphize, ADR 0012), so the
only design question is where the stride lives and what an element
read produces.

## Decisions

1. **Stride is a compile-time constant; the header stays
   `{len, cap, data*}`.** No fourth header word — C's discipline:
   `sizeof(T)` is compile-time, and shared routines take the size as a
   parameter (the `qsort`/stb_ds model). Elements are inline at
   `data + i * stride`, stride = 8 × the element kind's words
   (ADR 0014 decision 2, now real).
2. **Two push routines.** Word elements keep `ys_push(hdr, value)`
   untouched — the bench-measured fast path. Multi-word elements call
   `ys_push_n(hdr, src*, stride_bytes)`: same doubling growth (min 4),
   `realloc(data, new_cap * stride)`, then one memcpy of stride bytes
   into `data + len * stride`. The pushed value snapshots at argument
   evaluation like every call argument, which also makes
   `push(xs, xs[0])` safe across the realloc.
3. **Element reads produce interior pointers; consumers copy** —
   exactly the struct-field contract. `ps[i].x` is lea + load, no
   whole-element copy. The realloc hazard (a later push invalidating
   the pointer) is closed by the existing copy discipline: every
   consumer copies at evaluation (let, assign, call argument,
   equality's left snapshot), so no interior pointer outlives an
   intervening mutation.
4. **Element writes memcpy.** `xs[i] = v` keeps ADR 0008's bounds
   check, evaluates and snapshots `v` first (an index expression in the
   target may push — same discipline as decision 2), then copies stride
   bytes to `data + i * stride`.
5. **`for x in xs` copies the element out per step** (ADR 0010/0014
   live iteration): length re-read each iteration, then a stride-wide
   copy into the loop temp before the body runs. An optional element
   type gives the loop variable Opt-shaped storage, so narrowing
   unwraps it like any binding (ADR 0021 decision 3).
6. **The `int?[]` gate lifts.** Elements now occupy their full Opt
   width — a real tag word — so a pushed payload word can no longer
   alias null; the 0-aliases-null problem was an artifact of the
   word-only buffer. Null-element literals (`[1, null]`, checker-typed
   `int?[]`) lower by wrapping each element at the store, the fits
   rule's array seat (ADR 0021 decision 4).
7. **Unchanged:** array equality stays handle identity, `len` stays one
   load, bounds-check policy stays ADR 0022. Printing arrays stays
   gated with aggregate printing.

## Consequences

**Positive:** the ADR 0014 promise lands — value-struct arrays are C
arrays, contiguous and prefetchable; `string[]` and `int?[]` compile;
after this the `not yet compilable` surface is parameters beyond six,
aggregate printing, and float formatting.

**Accepted costs:** one multiply in the index path for strides the
addressing modes can't scale (word arrays keep the `,8` form);
multi-word push pays a snapshot plus one memcpy; a second runtime
symbol (`ys_push_n`) joins the inventory.
