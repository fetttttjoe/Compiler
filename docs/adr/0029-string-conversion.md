# ADR 0029 — string(x) Conversion and the Shared Text Builder

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0013 (concat is the allocating op), ADR 0025 (show
  routines), ADR 0027 (float text), ADR 0028 (type-call conversions)

## Context

String building (ADR 0013's named seat) is half-shipped: `+`
concatenation works end-to-end, but no value converts to text except
through `print`. The compiled text producers all print — printf
fragments in the show routines, printf inside `ys_fmt_f64` — so there
is no path from a value to string *bytes*.

## Decisions

1. **Syntax: `string(x)`** — the ADR 0028 type-call pattern with the
   third type keyword. `Expr::Convert` carries a three-way target
   (`int`/`float`/`string`) instead of a bool.
2. **`string(x)` produces exactly `print(x)`'s text** — the
   interpreter's `Value::display` (depth budget 8, name-sorted fields,
   refstruct hop = 2 levels) is the normative definition, so
   `print(string(x))` and `print(x)` write identical bytes for every
   convertible value.
3. **Every value type converts except the no-ops**: `string(s)` on a
   `string` is an identity conversion and is rejected (ADR 0028 —
   no-ops don't parse as meaning), `unit` and `null` have no value to
   convert. Narrowing applies: a narrowed `string?` is a `string`.
4. **One text sink in compiled code: a static byte builder** in .bss
   (`{len, cap, ptr}`, doubling growth, never freed — the ADR 0015
   arena story; static is sound because compiled programs are
   single-threaded, the ADR 0027 scratch precedent). Producers append:
   `ys_sb_append`, `ys_sb_int`, a reworked `ys_fmt_f64` (same probe
   algorithm, fragments append instead of printf), and the show
   routines. Consumers reset, produce, then use the bytes: `print`'s
   float/aggregate arms printf `%.*s\n` once; `string(x)` copies out
   to an exact-length heap string. Argument evaluation completes
   before the reset and show routines call no user code, so builder
   uses never interleave.
5. **`string(x)` allocates one exact-length string per conversion** —
   a visible cost at a visible site (ADR 0013 decision 4).
   `string(bool)` allocates nothing: it selects a static interned
   `"true"`/`"false"` descriptor.
6. **Print's scalar fast paths stay direct printf** (int, bool, str,
   null, unit, scalar optional payloads): identical text by
   construction, no builder traffic.

## Consequences

**Positive:** the string seat closes — conversion plus the existing
`+` builds strings; one renderer means print and `string()` can never
diverge; the float bit-pattern harness now guards both consumers of
`ys_fmt_f64`; future text features (interpolation as parser sugar)
need no new machinery.

**Accepted costs:** a global builder bakes in the single-thread
assumption already made by ADR 0027; each `string()` mallocs (no
reuse across conversions — the arena/leak story); recursive value
structs share `print`'s permanent `not yet compilable` diagnostic.
