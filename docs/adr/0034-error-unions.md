# ADR 0034 — Error Unions: `T!`, `error` Codes, and `try`

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0005 (explicitness), ADR 0012 (no boxing), ADR 0021
  (the tag+payload pattern this rides), ADR 0022 (the trap contract it
  does not touch), ADR 0029 (show routines), ADR 0031 (whose
  failure-as-value precedent this gives an identity), ADR 0032
  (additive-only), ADR 0033 (the narrowing this depends on)

## Context

Failure today is `null`/`false`: existence without identity, and
propagation is a manual guard ladder per call. Phase D names error
handling the first big rock. Exceptions are out — hidden control flow
(ADR 0005). `Result<T, E>` is out *for now* — it needs generics *and*
user-defined sum types, neither of which exists; waiting stacks the
language's most-needed feature behind its two largest. Optionals
already proved the alternative: special-case the one union you need,
monomorphic and generics-free, and let the general mechanism arrive
later without conflict. Errors get the optional treatment.

## Decisions

1. **Declarations.** `error NotFound;` (comma lists allowed:
   `error NotFound, Timeout;`) at module level, module-scoped
   `(module, name)` like structs, `export`/`import` like everything
   else. The same name in two modules is two distinct errors. Codes
   are interned deterministically at check time (module index, then
   declaration order), start at 2, and are never observable — only
   names render.
2. **`error` is a first-class one-word type** — the code. Bindable,
   passable, returnable; `==`/`!=` compare code identity.
   `error.Name` is the literal.
3. **`T!` is T-or-error.** v1 positions: function return types and
   `var`/`const` annotations — the flow positions. Params, struct
   fields, and array elements get a clean "error unions are not yet
   supported here" diagnostic, widened later (the representation is
   position-independent, so widening is mechanical). No `T!!`; `T?!`
   is parser-rejected in v1 with its tag state reserved (below).
4. **Narrowing mirrors null narrowing.** `x == error` narrows `x` to
   `error` in the true branch and to `T` in the false branch /
   fall-through; the divergence-aware machinery (ADR 0020) and the
   locals fix (ADR 0033) apply unchanged. Inside the error branch,
   `x == error.Name` distinguishes codes. The fact stack generalizes
   from "proven non-null" to a per-path narrowed state — one enum
   where a set was.
5. **`try e`** — `e` must be `T!`-typed; yields `T`; on error,
   returns the error from the enclosing function, whose declared
   return type must be some `U!` (checker-enforced diagnostic
   otherwise). Visible control flow at the exact site — the same
   standing `?.` already has under ADR 0005. No `catch`/default
   operator in v1: the narrowed `if` covers it; the operator can
   arrive additively (the `??` precedent). **As built, `try` is
   statement-positioned**: the direct right-hand side of a binding,
   assignment, or `return`, or a bare expression statement —
   `f(try g())` is a pointed diagnostic. The restriction is the
   interpreter's: expressions cannot early-return without threading a
   stop channel through every eval path; the lift is named (widen
   `Result`'s error side to `Diag | Propagate`) and purely additive.
   The backend has no such limit — compiled `try` is an inline tag
   test wherever it appears.
6. **Representation** (the ADR 0021 pattern): `T!` is
   `{tag, payload}`, tag first, `1 + words(T)` words. Tag 0 = value,
   tag 1 = **reserved** (null — the future `T?!`), tag ≥ 2 = the
   error code. Payload words are zeroed at error construction
   (canonical, memcmp-friendly, exactly like ADR 0021 null). Uniform
   for value- and ref-shaped `T` — a handle cannot encode a code, so
   ref-shaped `T!` still carries the tag word. Bare `error` is one
   word.
7. **Rendering.** `print`/`string()` of an `error` value produces
   `error.Name` — a show routine over interned name strings, selected
   by a compare chain (ADR 0029's pattern). **Whole-`T!` printing
   ships** (revised from the draft): the interpreter renders the
   unwrapped payload or the error name for free, and the compiled
   routine is one tag test over the existing optional pattern —
   gating it would have cost more than building it.
8. **`fun main(): int!` is legal** (with or without `args`). An
   escaping error prints `error: error.Name` on stderr and exits 1 —
   the trap shape (ADR 0022) minus the location line: a bare code
   carries none; locations and traces arrive with payloads (below).
   Such programs are diff/CLI-tested; the corpus stays `=> Int(n)`
   programs, so the freeze is untouched by construction.
9. **Builtins are unchanged.** The ADR 0032 freeze pins their
   signatures; error-returning I/O variants arrive later as new
   builtins when real programs want them.
10. **Forward seams, named now** so later work integrates without
    re-layout: payload declarations `error NotFound(string);` put
    payload words after the tag where optionals put `T`'s, and the
    interned table grows a layout column; a `catch`-style default
    operator; error-set refinement has syntax room after the postfix
    sigil (`int!{NotFound, Timeout}`); `T?!` occupies tag 1; generics
    compose by pre-check substitution (`T!` → `int!`), zero
    interaction. Nothing in v1 may assume an error value is one word
    *forever* — the code is the identity, not the whole story.

## Consequences

**Positive:** failure gets identity and one-keyword propagation with
no unwinding machinery in binaries — a tag test and a branch, the
same lowering shape optionals proved; the interpreter's
`Value::Err(code)` is the normative semantics; the differential
contract extends by the existing playbook (show routines, trap-shaped
exits, diff harness).

**Accepted costs:** `error` and `try` become keywords — a post-1.0
surface change; the corpus, examples, and benches were audited clean,
so no golden moves, but user programs using those identifiers must
rename. Two special-cased unions (optional, error) instead
of one general mechanism — accepted deliberately; the general one
(generics + sum types) can still arrive and interoperate. The checker
carries a second narrowing state kind. Error identity is nominal and
module-scoped: two modules wanting to share `NotFound` share it by
`import`, not by spelling.
