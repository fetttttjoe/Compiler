# ADR 0002 — Base Types, Operators, and Control Flow

- **Status:** Accepted
- **Date:** 2026-07-07
- **Extends:** ADR 0001 (supersedes its "comparison/logical operators are
  rejected until booleans land" interim rule — they are now live)

## Context

ADR 0001 shipped the pipeline with `int`/`float` only; comparison and logical
operators parsed but were rejected by `check`, and there was no control flow.
This ADR records the language-defining decisions made when `bool`, `string`,
the full operator set, and `if`/`while` landed.

## Decisions

1. **Strict `bool` conditions — no truthiness.** `if count { … }` is a type
   error; write `count != 0`. Stricter than TypeScript by intent: truthiness
   can be added later as sugar, but the reverse migration can't.
2. **Operator typing.**
   - `== !=`: same-type primitives (`int`, `float`, `bool`, `string`) → `bool`.
     No struct equality, no cross-type comparison, no coercion.
   - `< <= > >=`: same-type numerics → `bool`.
   - `&& || !`: `bool` only, and `&&`/`||` **short-circuit** (the right side
     evaluates only when the left hasn't decided the result).
   - `+` concatenates `string + string`; no other string operators in v1.
   - Precedence (C-family): `||` < `&&` < equality < comparison < additive <
     multiplicative < unary < postfix. Binding powers stay derived from the
     `Precedence` enum's declaration order, now with a compile-time assert
     keeping prefix/postfix above every infix level.
3. **Strings** are double-quoted and single-line, with `\"  \\  \n  \t`
   escapes; unterminated literals and unknown escapes are diagnostics with
   recovery.
4. **No struct literals bare in a condition** (Rust's rule). `if x { … }`
   always reads `x` as the condition; parentheses re-enable literals
   (`if (P { x: 1 }).x == 1 { … }`). This resolves the `Ident {`
   ambiguity that struct literals and blocks share.
5. **Blocks scope.** `if`/`while` bodies introduce child scopes in both the
   checker and the interpreter: inner bindings shadow and expire at `}`;
   assignment writes through to the binding's own scope.
6. **Definite-return analysis.** A non-`unit` function must return on every
   path: an `if` counts only when both branches exist and both return; a
   `while` never counts (its condition can be false on entry). This also
   closed a pre-existing soundness hole where `fun f(): int {}` type-checked
   and produced `unit` at runtime.
7. **Statement-level error recovery.** All blocks (function bodies, `if`,
   `while`) parse through a shared `parse_block` that, after a malformed
   statement, skips to the next `;` or stops before a statement keyword or
   `}` — one diagnostic per broken statement instead of a cascade, without
   eating the block's closing brace.
8. **`else if`** is not a special form: it parses as an `else` body containing
   a single nested `if`.

## Consequences

**Positive:** real programs run (recursion + `while` + `if` verified end to
end); the checker rejects every ill-typed operator/condition combination with
a located diagnostic; scoping is consistent between checking and runtime;
84 tests cover both accept and reject paths.

**Accepted costs:** no truthiness sugar (revisit only with evidence it's
missed); string ordering (`<` on strings) rejected for now; struct equality
deferred until struct runtime values land; `while true { return 1; }` is a
definite-return error even though it can't fall through (conservative
analysis — matches what a future codegen can easily verify).

**Deferred next:** struct values at runtime (`Value::Struct`), `else`-less
match-style constructs, `for`, string interpolation, codegen (per ADR 0001).
