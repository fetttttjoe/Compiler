# Ys — VS Code syntax highlighting

TextMate grammar for `.ys` files. No language server: this is pure declarative
highlighting (keywords, types, strings, comments, numbers, operators, function
names), the same mechanism most languages use for their baseline coloring.

## Install (local)

Symlink this folder into your VS Code extensions dir, then reload:

```sh
ln -s "$(pwd)/editors/vscode" ~/.vscode/extensions/ys-language
```

Or press `F5` on this folder in VS Code to launch an Extension Development Host
with the grammar loaded.

## Keeping it in sync

`syntaxes/ys.tmLanguage.json` is **generated** from the `KW_*` constants in
`src/syntax.rs` — don't hand-edit it. After adding a keyword to the lexer:

```sh
python3 tools/gen-grammar.py          # regenerate the grammar
python3 tools/gen-grammar.py --check   # CI/pre-commit: fail if it's stale
```

If you add a keyword to `syntax.rs` without categorizing it in the generator,
`gen-grammar.py` errors loudly rather than leaving it uncolored. Operators,
strings, comments, and numbers are a fixed template in the generator (multi-char
operators are assembled in the lexer, not stored as constants).

## Upgrade path

For context-aware coloring (a call vs. a definition, unused variables, type
errors inline), the next step is an LSP server emitting semantic tokens — it can
reuse the compiler's own lexer/checker. Not needed for plain highlighting.
