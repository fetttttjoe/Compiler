# ys

A statically typed language that reads like TypeScript and runs like C:
ahead-of-time compiled to x86-64 machine code through the system `cc`,
with no GC, no boxing, and no implicit conversions.

```ys
export refstruct Tree { value: int, left: Tree?, right: Tree? }

export fun sum(tree: Tree?): int {
    if tree == null {
        return 0;
    } else {
        return sum(tree.left) + tree.value + sum(tree.right);
    }
}
```

## Features

- strict scalars — `int`, `float`, `bool`, immutable `string`; no
  coercions, no truthiness
- value-copying `struct` and explicitly aliased `refstruct`
- optionals — `T?`, `null`, `?.`, `??` — with flow-sensitive narrowing
- growable arrays with `len`, `push`, and live `for` loops
- loop control with `break` and `continue`
- modules with explicit `import` / `export`
- every binding declares its type; the compiler verifies, it never
  guesses

## Getting started

Requirements: a stable Rust toolchain; native builds also need a system
`cc` on x86-64 Linux.

Interpret a program:

```sh
cargo run --quiet -- examples/tree/main.ys
```

```text
tree total
37
tree minimum
1
=> Int(37)
```

Compile and run a native binary — `main`'s return value is the exit
code:

```sh
cargo run --quiet -- build examples/tree/main.ys -o /tmp/ys-tree
/tmp/ys-tree
echo $?  # 37
```

## Architecture

```text
lexer → parser → checker → { interpreter | IR → regalloc → assembly → cc }
```

Two engines, one semantics: the interpreter is the normative
specification, and every compiled program is continuously checked
against it — identical stdout, identical exit code. Design decisions
are recorded in `docs/adr/`.

## Status

Pre-1.0. Anything the checker accepts but the backend cannot compile
yet fails with a clean `not yet compilable` diagnostic — there is no
fallback path and no silent corruption. Allocations currently live
until process exit; region memory is the planned direction.

## Development

```sh
cargo test                            # unit, CLI, differential, conformance
cargo fmt --check
cargo clippy --all-targets -- -D warnings
python3 tools/fuzz.py                 # differential fuzzing of both engines
./benches/run.sh                      # paired ys/C benchmarks vs -O2
python3 tools/gen-grammar.py --check  # editor grammar in sync with the parser
```

`cargo run --quiet -- ir <file.ys>` prints the backend's
pre-register-allocation IR — developer output, not a stable format.
