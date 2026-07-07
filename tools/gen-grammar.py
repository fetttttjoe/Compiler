#!/usr/bin/env python3
"""Generate the VS Code TextMate grammar from the compiler's own keyword list.

Source of truth: the `KW_*` constants in `src/syntax.rs`. Add a keyword there,
run this, and the editor learns it too. If a keyword in syntax.rs isn't
categorized below, this errors instead of silently leaving it uncolored.

    python3 tools/gen-grammar.py          # (re)write the grammar
    python3 tools/gen-grammar.py --check   # fail if the committed grammar is stale (CI)

ponytail: keywords are generated (they drift); strings/comments/numbers/operators
are a fixed template — multi-char operators live in the lexer, not as constants,
so there's nothing clean to generate them from.
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SYNTAX = ROOT / "src" / "syntax.rs"
GRAMMAR = ROOT / "editors" / "vscode" / "syntaxes" / "ys.tmLanguage.json"

# How each keyword spelling maps to a TextMate scope. Every KW_ in syntax.rs
# must appear in exactly one bucket (checked below).
CATEGORIES = {
    "storage.type.ys": ["fun", "struct", "var", "const"],
    "keyword.control.ys": ["return", "if", "else", "while", "import", "export", "from"],
    "support.type.primitive.ys": ["int", "float", "bool", "string"],
    "constant.language.ys": ["true", "false"],
}


def keywords_from_syntax():
    """`{name: spelling}` for every `pub const KW_X: &str = "spelling";`."""
    text = SYNTAX.read_text()
    return dict(re.findall(r'pub const KW_(\w+):\s*&str\s*=\s*"([^"]+)"', text))


def keyword_patterns(spellings):
    """One TextMate rule per category, plus a coverage check against syntax.rs."""
    categorized = {kw for kws in CATEGORIES.values() for kw in kws}
    missing = spellings - categorized
    if missing:
        raise SystemExit(
            f"error: keyword(s) {sorted(missing)} are in src/syntax.rs but not "
            f"categorized in {Path(__file__).name} — add them to CATEGORIES."
        )
    extra = categorized - spellings
    if extra:
        raise SystemExit(
            f"error: keyword(s) {sorted(extra)} are categorized but no longer in "
            f"src/syntax.rs — remove them from CATEGORIES."
        )
    rules = []
    for scope, kws in CATEGORIES.items():
        alt = "|".join(sorted(kws))
        rules.append({"name": scope, "match": rf"\b({alt})\b"})
    return rules


def build_grammar():
    keywords = keywords_from_syntax()
    kw_rules = keyword_patterns(set(keywords.values()))
    # The definition rule tracks KW_FUN's spelling, not a hardcoded "fun".
    kw_fun = keywords["FUN"]
    return {
        "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
        "name": "Ys",
        "scopeName": "source.ys",
        "patterns": [{"include": f"#{n}"} for n in (
            "comments", "strings", "function-definition", "keywords",
            "numbers", "function-call", "operators",
        )],
        "repository": {
            "comments": {"name": "comment.line.double-slash.ys", "match": r"//.*$"},
            "strings": {
                "name": "string.quoted.double.ys",
                "begin": r'"',
                "end": r'"|$',
                "patterns": [
                    {"name": "constant.character.escape.ys", "match": r'\\["\\nt]'}
                ],
            },
            "function-definition": {
                "match": rf"\b({kw_fun})\s+([A-Za-z_][A-Za-z0-9_]*)",
                "captures": {
                    "1": {"name": "storage.type.function.ys"},
                    "2": {"name": "entity.name.function.ys"},
                },
            },
            "keywords": {"patterns": kw_rules},
            "numbers": {"name": "constant.numeric.ys", "match": r"\b[0-9]+(\.[0-9]+)?\b"},
            "function-call": {
                "match": r"\b([A-Za-z_][A-Za-z0-9_]*)\s*(?=\()",
                "name": "entity.name.function.call.ys",
            },
            "operators": {
                "name": "keyword.operator.ys",
                "match": r"==|!=|<=|>=|&&|\|\||[-+*/%=<>!]",
            },
        },
    }


def main():
    grammar = build_grammar()
    rendered = json.dumps(grammar, indent=2, ensure_ascii=False) + "\n"
    if "--check" in sys.argv:
        current = GRAMMAR.read_text() if GRAMMAR.exists() else ""
        if current != rendered:
            raise SystemExit(
                f"error: {GRAMMAR.relative_to(ROOT)} is stale — run "
                f"`python3 tools/gen-grammar.py` and commit."
            )
        print("grammar is up to date with src/syntax.rs")
        return
    GRAMMAR.write_text(rendered)
    print(f"wrote {GRAMMAR.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
