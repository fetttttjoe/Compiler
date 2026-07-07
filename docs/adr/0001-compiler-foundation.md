# ADR 0001 — Compiler Foundation Architecture

- **Status:** Accepted
- **Date:** 2026-07-06
- **Scope:** the full front-end + reference interpreter shipped on `main`
  (spans, diagnostics, lexer, Pratt parser, type checker, tree-walking
  interpreter, test suite, CI gates)

## Context

The long-term goal is a language *"as easy as TypeScript, but precompiled"* —
approachable, statically typed, compiled ahead of time to native/WASM. The
original prototype had structural problems that blocked that path: the lexer
tracked brace nesting (parser's job), expressions never became a tree
(precedence was recorded but unused), the AST embedded lexer tokens, there
were no source positions, no error type, and no tests. Backwards compatibility
was explicitly waived, so the foundation was rebuilt rather than patched.

## Architecture

```
source ─lex─▶ Vec<Token> ─parse─▶ Ast ─check─▶ well-typed Ast ┬─ interpret ─▶ Value        (reference / tests)
                                                              └─ [codegen] ─▶ native/WASM   (reserved seat)
```

```rust
pub fn lex(source: &str)       -> (Vec<Token>, Vec<Diagnostic>);   // lexer.rs
pub fn parse(tokens: &[Token]) -> (Ast, Vec<Diagnostic>);          // parser.rs
pub fn check(ast: &Ast)        -> (SymbolTable, Vec<Diagnostic>);  // check.rs — gates the pipeline
pub fn interpret(ast: &Ast)    -> Result<Value, Diagnostic>;       // interpreter.rs
```

Modules form a strict downward dependency DAG:

| Module           | Layer    | Responsibility                                                      |
| ---------------- | -------- | ------------------------------------------------------------------- |
| `syntax.rs`      | L0 leaf  | the lexical alphabet: every character/keyword constant + line-ending policy |
| `span.rs`        | L0 vocab | byte-offset `Span`; `LineIndex` (offset → line:col at render time)  |
| `diagnostic.rs`  | L0 vocab | `Diagnostic { severity, message, span }` + rendering                |
| `token.rs`       | L1 data  | `TokenKind`, `Token { kind, span }`                                 |
| `ast.rs`         | L1 data  | tree types; owns `BinOp`/`UnOp` — never sees `TokenKind`            |
| `lexer.rs`       | L2 logic | source → tokens, diagnostics accumulate + recover                   |
| `parser.rs`      | L2 logic | Pratt parser, consume-and-check, recovery at boundaries             |
| `check.rs`       | L2 logic | static types; produces `SymbolTable` (future codegen input)         |
| `interpreter.rs` | L2 logic | tree-walking reference semantics, fail-fast                         |

## Decisions

1. **Pure-phase contracts, no traits.** Each phase is a pure function; the
   shared types + signatures above *are* the interface. A trait is introduced
   only when a second implementation actually exists.
2. **Hand-rolled, zero runtime dependencies.** No lexer/parser generators —
   the internals are the point of the project. Tests are inline
   `#[cfg(test)]` modules with plain `assert_eq!` (parser structure asserted
   via a test-only s-expression rendering).
3. **Byte-offset spans everywhere.** Every token, AST node, and diagnostic
   carries a `Span`; line/column is computed only when rendering, via
   `LineIndex`. Lexer and parser accumulate diagnostics and recover (many
   errors per run); recovery always consumes at least one token. The
   interpreter is fail-fast.
4. **The lexical alphabet is single-sourced (`syntax.rs`).** No raw character
   or keyword literals in the lexer body — every `'('` and `"fun"` is a named
   constant. Line endings (`\n`, `\r\n`, lone `\r`) are one line break,
   implemented once and shared by the lexer and `LineIndex`; a `LineEnding`
   enum (reserved) names the style for future round-tripping.
5. **Named precedence, derived binding powers.** Operator precedence is a
   `Precedence` enum whose declaration order *is* the ranking; Pratt binding
   powers are derived from it (`left_bp`/`right_bp`), plus `PREFIX_BP` /
   `POSTFIX_BP` constants. The AST owns its operator enums; the parser
   translates tokens at the boundary.
6. **Statically typed from day one.** `check` is a built phase, not a future
   seat: signatures are collected first (order-independent), then bodies are
   checked with binding types inferred from initializers, no implicit
   int↔float coercion, `const` immutability, and call/field/struct-literal
   shape checking. Only programs with zero diagnostics reach the interpreter,
   so runtime type errors are unreachable in the normal pipeline.
7. **The interpreter is the reference semantics, not a throwaway.** Once a
   backend exists it becomes the differential-testing oracle for compiled
   output. `codegen(ast, symbols) -> Vec<u8>` is a named, unbuilt seat —
   `SymbolTable` (resolved signatures + struct layouts) is already its input.
8. **v1 semantics notes.** Integer arithmetic wraps (`wrapping_*`); division
   by zero is a runtime diagnostic. Comparison/logical operators (`< > && ||
   !`) parse but are rejected by `check` until booleans + control flow land —
   they will be introduced in `check` and `interpret` together.
9. **CI gates:** `cargo build`, `cargo test`, `cargo clippy --all-targets --
   -D warnings` on every push/PR to `main`.

## Consequences

**Positive:** every phase is testable in isolation against its contract
(56 tests, error paths included); phase internals can be rewritten without
touching consumers; new phases (checker was added mid-build) slot in without
restructuring; diagnostics always carry a source location.

**Accepted costs:** the check/interpret pair intentionally duplicates the
"not yet supported" operator rejections until booleans land; wrapping integer
semantics are provisional; statement-level error recovery inside function
bodies is coarse (one malformed statement can emit several diagnostics) and
is deferred to the control-flow work, where recovery semantics get designed
properly.

**Deferred (each has a named seat):** booleans/comparisons/control flow
(parser + check + interpret), struct values at runtime (`Value::Struct`),
codegen (reserved contract), pretty caret diagnostics (`diagnostic.rs`
rendering), CLI file input / REPL (`main.rs`).

> **Amendment (2026-07-07):** booleans/comparisons/control flow landed (ADR
> 0002), and caret-underline diagnostic rendering landed in
> `Diagnostic::render` — both rows above are done. CLI file input landed with
> multi-file compilation (ADR 0003). Struct runtime values landed
> (`Value::Struct` in `interpreter.rs`). Remaining seats: codegen, REPL.
