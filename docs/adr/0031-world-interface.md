# ADR 0031 — World Interface: main Args, stdin, and File Handles

- **Status:** Accepted
- **Date:** 2026-07-15
- **Extends:** ADR 0013 (strings are raw bytes), ADR 0015 (leak by
  design), ADR 0017 (Phase C), ADR 0021 (ref-shaped optionals),
  ADR 0022 (runtime-error class)

## Context

print + exit code is the entire world interface. Programs need
arguments, stdin, and files. ys has optionals but no exceptions, no
destructors, and never frees — so file handles are the language's
first *resource*, and failure needs a representation that isn't a
throw.

## Decisions

1. **`fun main(args: string[]): int`** — the entry point optionally
   takes exactly one `string[]`; `main(): int` stays legal. `args`
   excludes the program name (Deno convention). The interpreter CLI
   passes everything after the entry file through
   (`Compiler prog.ys a b c`); compiled binaries take argv natively.
2. **`file` is an opaque handle type** — a word, like refstruct
   handles, so `file?` rides the ref-shaped optional machinery
   (ADR 0021) unchanged: `open` returns `file?`, null tests narrow,
   equality is identity. `print`/`string()` render the constant text
   `file` — a compiled `FILE*` address could never match the oracle.
3. **Environmental failure is a value, buggy use is a trap.**
   `open` → null, `read`/`readLine` → null at EOF, `write`/`close` →
   false on failure: all handleable in-language. But read/write/close
   on an already-closed file and `read` with `max <= 0` are program
   bugs — ADR 0022 runtime errors (`operation on closed file`,
   `read size must be positive`), message + `file:line:col`, exit 1,
   identical in both engines.
4. **The compiled handle is a heap box `{FILE*, closed}`**, not the
   raw `FILE*`: use-after-close on a raw pointer is UB, and the
   interpreter diagnoses it — the box is what keeps the differential
   contract intact. One indirection per op, noise next to the I/O.
5. **Builtins** (shadowable, like print/len/push):
   `open(path: string, mode: string): file?` with mode pinned to
   `"r" | "w" | "a"` — both engines validate the set themselves, so
   fopen's extended modes can't cause divergence;
   `read(f: file, max: int): string?` — up to max bytes, fread
   semantics (the interpreter loops short reads to match); n = 0 is
   null, anything read is returned;
   `readLine(f: file): string?` and `readLine(): string?` (stdin) —
   one trailing `\n` stripped, `\r` kept (strings are raw bytes),
   null at EOF;
   `write(f: file, s: string): bool` — fwrite **plus fflush**: Rust
   `File` writes are unbuffered, so flushing per write is what makes
   write-then-reopen-then-read agree across engines;
   `close(f: file): bool` — marks the box closed, reports fclose.
6. **No whole-file slurp builtins.** Huge files were the explicit
   concern; streaming via handles (or stdin redirection) is the
   supported path. Convenience slurps want a string-builder type and
   can arrive with it. Unclosed handles leak fds at exit — the
   ADR 0015 story applied to its first non-memory resource; an
   explicit close/defer discipline is the program's job.
7. **`ys_args` materializes argv once** at entry when main takes the
   parameter: an ordinary string array (descriptor elements,
   ADR 0023 layout) built from `argv[1..]` with strlen lengths.

## Consequences

**Positive:** real filters and tools are writable (args, stdin
loops, file round-trips); every new failure mode is either a null an
`if` can handle or a trap the harness pins; `file` composes with
structs, arrays, and optionals for free because it is just a word.

**Accepted costs:** `file` becomes a keyword (pre-1.0 break);
per-write fflush trades batching for engine parity (a buffered
writer type can lift it later); fd exhaustion from unclosed handles
is the program's bug, mirroring the memory story; the old
"more than one CLI argument is an error" behavior is gone — trailing
arguments now belong to the program.
