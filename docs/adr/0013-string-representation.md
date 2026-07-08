# ADR 0013 — String Representation: Immutable Fat Pointers

- **Status:** Accepted — landed: literals are fully static rodata
  `{ptr, len}` descriptors (deduplicated), copies ride the value-struct
  machinery, equality is length-then-memcmp, `print` formats via printf
- **Date:** 2026-07-08
- **Extends:** ADR 0005 (nothing implicit — including hidden copies),
  ADR 0012 (laws 1 and 2: natural layout, stated before the feature
  reaches codegen)

## Context

`str` exists in the checker and interpreter (`Value::Str`, cloned on
copy), but its compiled representation is undecided. Inheriting "value
semantics = deep copy" would make every string assignment and argument
pass a hidden O(n) allocation — precisely the implicit machinery ADR
0005 forbids. The representation must be pinned before strings reach
codegen or concatenation is designed.

## Decisions

1. **`str` is an immutable string: a fat pointer `(ptr, len)`.** Copying
   a string copies 16 bytes, always — O(1), two registers. There is no
   mutation through a `str`, which is what makes the cheap copy sound.
2. **Literals point into read-only data.** A string literal costs no
   allocation at runtime; its bytes live in the binary's rodata.
3. **Equality is content equality** — length check, then memcmp. That
   matches value-semantics intuition; identity comparison does not exist
   for strings (nothing observable distinguishes it).
4. **Concatenation, when it lands, allocates a new string at an explicit
   operation** — a visible cost at a visible site, never in-place
   mutation.
5. **Mutable string building is a separate future type** (`strbuf` or a
   library type) with its own ADR when a real program needs it.
6. **No null terminator.** Length-carried strings; C-interop conversion
   is a boundary concern for a future FFI ADR.
7. **The interpreter needs no change.** Rust `String` clones are
   observationally identical to immutable value strings — the oracle
   already models these semantics.

## Consequences

**Positive:** passing and returning strings is free; substring/slicing
can later be zero-copy views (safe exactly because content is
immutable); the layout is two registers, friendly to the future SSA IR.

**Accepted costs:** building strings piecewise is awkward until the
builder type exists; FFI needs an explicit conversion at the boundary
(deferred with FFI itself).
