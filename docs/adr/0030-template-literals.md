# ADR 0030 — Template Literals: Backticks Desugaring to Concat

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0013 (concat), ADR 0029 (string(x), the builder)

## Context

`string(x)` plus `+` builds strings, but interpolation-heavy code
reads badly as concat chains. TypeScript surface (the language's
identity) spells this `` `a ${x} b` ``.

## Decisions

1. **Backtick templates, TS syntax**: `` `a ${x} b` ``. Plain `"..."`
   strings keep their meaning untouched. Template text is single-line,
   like string literals — lifting that later is non-breaking.
2. **A template is pure parser sugar**: text runs become string
   literals, each `${e}` an *implicit* `string(e)`, the whole folded
   left over `+` concat. The checker, both engines, and every tool
   downstream only ever see the desugar — no new semantics, no new
   runtime machinery, differential parity by construction.
3. **Implicit conversions pass the string identity through**: `${s}`
   on a `string` concatenates it directly (TS behavior). The explicit
   `string(s)` stays a rejected no-op (ADR 0029). `unit` and `null`
   still don't interpolate — `cannot interpolate unit in a template`.
4. **Escapes**: the string set (`\"`, `\\`, `\n`, `\t`) plus `` \` ``
   and `\$`. A lone `$` is literal text; only `${` opens an
   interpolation.
5. **Lexer mode stack, JS-spec token split**: a template with no
   interpolation is an ordinary `StringLiteral`; otherwise
   `TemplateHead`/`TemplateMiddle`/`TemplateTail` carry the text and
   the code between them lexes normally. A per-template brace counter
   keeps struct literals working inside `${}`, and the stack makes
   templates nest.
6. **Spans stay unique in the span-keyed type table**: each implicit
   conversion spans its `${...}` delimiters (never colliding with its
   argument), text keeps its token span, and each fold node runs from
   the opening backtick to its rightmost part.

## Consequences

**Positive:** interpolation of anything printable — aggregates,
optionals, nested templates — with zero backend work; `print`,
`string()`, and templates can never disagree on text.

**Accepted costs:** template text is single-line for now; diagnostics
inside `${}` are phrased for interpolation while spans point into the
template; an unterminated template cascades parser recovery like an
unterminated string does.
