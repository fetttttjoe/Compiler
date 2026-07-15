# ys — TypeScript surface, C economics

A statically typed language (`.ys`) that reads like TypeScript and runs
like C: ahead-of-time compiled to x86-64 via the system `cc`, no GC, no
hidden machinery. Two engines, one semantics:

- **Interpreter** (`src/interpreter.rs`) — the *normative spec* (ADR
  0017). Where behavior is in question, its answer is the language's
  answer. Its bugs are spec errata, fixed with an ADR note.
- **Compiler backend** (`src/ir/` + `src/codegen.rs`) — one lowering
  to a vreg IR, linear-scan regalloc, AT&T assembly (ADR 0016/0018).

## Architecture

```
lexer → parser → check (+narrow) → { interpreter | ir → codegen } → cc
```

- `check.rs` owns ALL types. It exports `Resolutions` (signatures,
  struct layouts, per-expression type table, let annotations). The
  backend reads types; it never derives one. Keep the type table total —
  any expression the checker types without recording breeds fallback
  bugs downstream.
- `src/ir/` is the only backend. Multi-word values (structs, strings)
  travel as pointers; copies happen exactly where the oracle copies
  (let, assign, return, each call arg at evaluation, equality's left
  operand). Evaluation order must match the oracle — side effects are
  observable via `print`.
- Spans are file-global byte offsets: unique program-wide, safe as map
  keys across modules.
- Runtime errors the interpreter diagnoses (div-by-zero, i64::MIN/-1,
  out-of-bounds, invalid float→int conversion) print the same message
  plus `file:line:col` on stderr and exit 1 in compiled code too
  (ADR 0022/0028). Remaining signal cases:
  allocation exhaustion and native stack overflow — accepted, own ADR
  when their trigger fires.

## Non-negotiable invariants

1. **Nothing implicit** (ADR 0005): no coercions, no truthiness, no
   hidden allocation or control flow. Explicit beats implicit is the
   tiebreaker for every design question.
2. **Value/ref split is API** (ADR 0006/0012): `struct` copies, values
   never alias; `refstruct`/arrays are handles. The optimizer's
   correctness leans on this — never weaken it.
3. **No boxing, ever** (ADR 0012): every type has its natural machine
   layout; future generics monomorphize.
4. **The differential contract**: for any program the interpreter runs
   cleanly, the compiled binary produces identical stdout and exit code
   (result & 0xFF). tests/diff.rs and tests/conformance.rs enforce it.
5. **Goldens never move silently**: a change that alters any
   `conformance/*.out` updates the corpus in the same commit and says so
   in an ADR.

## Workflow

- Build/test: `cargo test` (unit + CLI + diff + conformance must all
  pass), `cargo clippy --all-targets` and `cargo fmt` clean.
- Benchmarks: `./benches/run.sh` — paired .ys/.c programs vs `-O2`;
  performance claims cite these numbers (ADR 0012).
- Fuzz: `python3 tools/fuzz.py` before merging backend changes. It
  covers ints/bools/arrays/calls — probe structs/strings/optionals
  manually when touching their lowering.
- Decisions live in `docs/adr/` (numbered, short, decisions +
  consequences). Read 0005, 0012, 0016–0018 before structural work.

## Adding a language feature (ADR 0018 recipe)

1. ADR with the semantics AND the memory-layout/lowering story.
2. Checker: typing rules; keep the per-expression type table total.
3. Interpreter: the normative semantics — this IS the spec.
4. Lowering in `src/ir.rs`; anything unsupported is a clean
   `not yet compilable` diagnostic, never a fallback.
5. Conformance corpus files (`conformance/*.ys` + `.out`) plus diff
   tests for edge cases, in the same commit; run the fuzzer.

Anything skipping a step isn't done.

## Commits

Max 2 lines: a conventional-commit subject (`feat:`, `fix:`, `docs:`,
`chore:`, `refactor:`, `test:`) plus at most one body line.

## Style

Simplicity > complexity. Reuse before writing; stdlib before custom;
delete before adding. Comments state constraints the code can't show —
not what the next line does. Match the codebase's voice.
