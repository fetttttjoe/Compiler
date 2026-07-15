# ADR 0028 — int↔float Conversion: Type-Call Syntax, Checked Narrowing

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0017 (Phase C item), ADR 0022 (runtime-error class)

## Context

No path exists between `int` and `float` (the mandel benchmark works
around it with float-only arithmetic). Conversion must be spelled, not
inferred (ADR 0005), and the float→int direction is partial: NaN,
infinities, and |f| ≥ 2^63 have no integer value.

## Decisions

1. **Syntax: `float(x)` and `int(x)`** — Python/Go-style conversion
   calls. `int`/`float` are keywords, so the form is unshadowable and
   parses as its own expression node (`Expr::Convert`) in atom
   position; no builtin-resolution ambiguity, no new tokens.
2. **Typing is strict:** `float(x)` requires `int`, `int(x)` requires
   `float`. Identity conversions are rejected (`int(i)` is a no-op —
   explicit beats implicit means no-ops don't parse as meaning), with
   a help note naming the type it already is.
3. **`float(i)` is total:** nearest-even (`cvtsi2sd`), exactly Rust's
   `i as f64` — the interpreter and backend agree by construction.
4. **`int(f)` truncates toward zero and is checked:** valid iff
   `f >= -2^63 && f < 2^63` (NaN fails both compares). Anything else
   is a runtime error — `invalid float to int conversion` plus
   `file:line:col`, exit 1, the ADR 0022 class, consistent with
   division by zero. Compiled lowering leans on the hardware:
   `cvttsd2si` yields the 0x8000000000000000 sentinel exactly for
   NaN/out-of-range/`-2^63`; the sentinel branches to a bit-compare
   against `-2^63`'s representation (0xC3E0000000000000) — the one
   legal producer — and traps otherwise. The fast path is one compare.

## Consequences

**Positive:** Phase C shrinks to string interpolation and `main`
args; numeric code stops needing float-only workarounds; the trap
matches the interpreter's diagnostic byte-for-byte (CLI-pinned).

**Accepted costs:** one compare per `int(f)` on the fast path; a new
trap stub joins the inventory; two IR instructions (`IntToFloat`,
checked `FloatToInt`) ride through regalloc as non-call scratch ops,
like `DivChecked`.
