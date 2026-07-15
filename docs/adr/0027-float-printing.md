# ADR 0027 — Float Printing: Shortest Round-Trip in the Runtime

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0017 (render contract), ADR 0025 (show routines)

## Context

The last feature gate. The normative text (interpreter = Rust f64
`Display`) is: shortest round-trip digits, positional notation always
(`1e21` prints 22 digits, `f64::MAX` all 309), `1` not `1.0`, `-0`,
`NaN`/`inf`/`-inf`. No libc format produces this; the runtime needs a
formatter. Chosen: hand-written assembly in the in-assembly runtime —
keeps the all-assembly identity.

## Decisions

1. **`ys_fmt_f64(bits in %rdi)`** prints the text (no newline) on the
   same stdout stream as every print. Bits travel in an integer
   register, so no emit changes: `movq` moves XMM vregs and spill
   slots into `%rdi` alike, and aggregate show routines already load
   float fields as words.
2. **Digits by round-trip search, not by hand.** For precision
   0..=16: `snprintf("%.*e")`, `strtod` back, compare bits; first hit
   wins (17 significant digits always round-trip, so the loop
   terminates). Correctness lean: glibc's conversions are correctly
   rounded, and the closest shortest digit string is unique — if any
   L-digit string round-trips, the correctly-rounded one does and is
   what Grisu picks. Locale is safe: a C program starts in the "C"
   locale unless it calls `setlocale`, which ys programs never do.
3. **Positional rendering from `d[.ddd]e±XX`**: compact the mantissa
   digits (skip the point), parse the exponent, then at most three
   `printf` fragments — digits+zeros (E ≥ n−1), split digits around a
   point (0 ≤ E < n−1), or `0.`+zeros+digits (E < 0). A 336-byte
   rodata zero block covers the extremes (subnormal `5e-324` needs
   323 zeros; `f64::MAX` needs 292). Zero-length `%.*s` prints
   nothing, so no empty-run branches. Specials short-circuit on bit
   patterns before any libc call.
4. **Scratch buffers are static** (.bss): compiled programs are
   single-threaded; a stack buffer would only complicate the frame.
5. **Every float-print route flows through it**: bare `print(float)`
   calls it directly; `float?` routes through the aggregate path
   (the show tag-wrapper); show routines gain a `Type::Float` leg;
   the `contains_float` gate is deleted — with it, the printing gate
   entirely.

## Consequences

**Positive:** `not yet compilable` now names nothing but recursive
value structs (unconstructible values — a permanent diagnostic, not a
feature). Print of any type compiles.

**Accepted costs:** up to 17 snprintf/strtod probes per float printed
(print is not a hot path; typical values hit in ≤ 7); ~130 lines of
runtime assembly, mitigated by a differential bit-pattern harness
(seeded random f64s through both engines, byte-compared) on top of
the corpus; theoretical tie-rounding divergence between glibc and
Grisu is exactly what that harness exists to catch.
