//! The lexical alphabet: every character and keyword the lexer recognizes,
//! named once here instead of scattered as literals through the scanner, plus
//! the cross-platform line-ending policy shared with `LineIndex`. This is the
//! single place to change how the language is spelled or how lines end.

// --- Whitespace & line-ending characters ---
pub const SPACE: char = ' ';
pub const TAB: char = '\t';
pub const CR: char = '\r';
pub const LF: char = '\n';

// --- Punctuation ---
pub const LPAREN: char = '(';
pub const RPAREN: char = ')';
pub const LBRACE: char = '{';
pub const RBRACE: char = '}';
pub const LBRACKET: char = '[';
pub const RBRACKET: char = ']';
pub const COLON: char = ':';
pub const SEMICOLON: char = ';';
pub const COMMA: char = ',';
pub const DOT: char = '.';

// --- Operators ---
pub const EQUALS: char = '=';
pub const PLUS: char = '+';
pub const MINUS: char = '-';
pub const STAR: char = '*';
pub const SLASH: char = '/';
pub const PERCENT: char = '%';
pub const BANG: char = '!';
pub const LESS: char = '<';
pub const GREATER: char = '>';
pub const AMPERSAND: char = '&';
pub const PIPE: char = '|';
pub const QUESTION: char = '?';
pub const UNDERSCORE: char = '_';

// --- String literals ---
pub const QUOTE: char = '"';
pub const BACKSLASH: char = '\\';
/// Template literals (ADR 0030): `` ` `` delimits, `${` interpolates.
pub const BACKTICK: char = '`';
pub const DOLLAR: char = '$';
/// Escape names: `\n` and `\t` inside a string literal.
pub const ESCAPE_LF: char = 'n';
pub const ESCAPE_TAB: char = 't';

// --- Keyword spellings ---
pub const KW_FUN: &str = "fun";
pub const KW_STRUCT: &str = "struct";
pub const KW_REFSTRUCT: &str = "refstruct";
pub const KW_VAR: &str = "var";
pub const KW_CONST: &str = "const";
pub const KW_RETURN: &str = "return";
pub const KW_BREAK: &str = "break";
pub const KW_CONTINUE: &str = "continue";
pub const KW_INT: &str = "int";
pub const KW_FLOAT: &str = "float";
pub const KW_BOOL: &str = "bool";
pub const KW_STRING: &str = "string";
pub const KW_FILE: &str = "file";
pub const KW_ERROR: &str = "error";
pub const KW_TRY: &str = "try";
/// Payload enums and their consumption (ADR 0036).
pub const KW_ENUM: &str = "enum";
pub const KW_MATCH: &str = "match";
pub const KW_TRUE: &str = "true";
pub const KW_FALSE: &str = "false";
pub const KW_NULL: &str = "null";
pub const KW_IF: &str = "if";
pub const KW_ELSE: &str = "else";
pub const KW_WHILE: &str = "while";
pub const KW_FOR: &str = "for";
pub const KW_IN: &str = "in";
pub const KW_IMPORT: &str = "import";
pub const KW_EXPORT: &str = "export";
pub const KW_FROM: &str = "from";

// --- Language conventions ---
/// The program entry point's function name — the driver, checker, and
/// backend all resolve the entry through this one spelling.
pub const ENTRY_FN: &str = "main";

// --- Builtin function names ---
// Not keywords: a user definition of the same name shadows the builtin,
// so these resolve last in both the checker and the interpreter.
pub const BUILTIN_PRINT: &str = "print";
pub const BUILTIN_LEN: &str = "len";
pub const BUILTIN_PUSH: &str = "push";
/// The world interface (ADR 0031): files and stdin.
pub const BUILTIN_OPEN: &str = "open";
pub const BUILTIN_READ: &str = "read";
pub const BUILTIN_READLINE: &str = "readLine";
pub const BUILTIN_WRITE: &str = "write";
pub const BUILTIN_CLOSE: &str = "close";

/// True for a source line break (`\n` or `\r`). CRLF is handled by the caller
/// consuming the trailing `\n`.
pub fn is_line_break(c: char) -> bool {
    c == LF || c == CR
}

pub fn is_whitespace(c: char) -> bool {
    c == SPACE || c == TAB || is_line_break(c)
}

/// Byte offsets at which each line starts, recognizing `\n`, `\r\n`, and lone
/// `\r`. Always begins with 0. Shared by `LineIndex`.
pub fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let mut chars = source.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == LF {
            starts.push(i + LF.len_utf8());
        } else if c == CR {
            if chars.peek().map(|&(_, next)| next) == Some(LF) {
                let (j, lf) = chars.next().unwrap();
                starts.push(j + lf.len_utf8());
            } else {
                starts.push(i + CR.len_utf8());
            }
        }
    }
    starts
}

/// Line-ending style of a source file. The lexer and `LineIndex` treat all
/// three as one line break; `detect` records the dominant style.
///
/// Reserved: `detect`/`as_str` are the round-tripping API for a future
/// formatter / code generator, and are not yet wired into the pipeline — but
/// line-ending policy lives here so it has exactly one home.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

#[allow(dead_code)]
impl LineEnding {
    /// The style of the first line break in `source` (defaults to `Lf`).
    pub fn detect(source: &str) -> LineEnding {
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            if c == CR {
                return if chars.peek() == Some(&LF) {
                    LineEnding::CrLf
                } else {
                    LineEnding::Cr
                };
            }
            if c == LF {
                return LineEnding::Lf;
            }
        }
        LineEnding::Lf
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_starts_handles_lf_crlf_and_cr() {
        assert_eq!(line_starts("a\nb"), vec![0, 2]); // LF
        assert_eq!(line_starts("a\r\nb"), vec![0, 3]); // CRLF
        assert_eq!(line_starts("a\rb"), vec![0, 2]); // lone CR
        assert_eq!(line_starts("abc"), vec![0]); // no breaks
    }

    #[test]
    fn detect_reports_the_first_line_ending_style() {
        assert_eq!(LineEnding::detect("a\nb"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\rb"), LineEnding::Cr);
        assert_eq!(LineEnding::detect("no breaks"), LineEnding::Lf);
    }

    #[test]
    fn whitespace_classification() {
        assert!(is_whitespace(SPACE) && is_whitespace(TAB));
        assert!(is_line_break(LF) && is_line_break(CR));
        assert!(!is_whitespace('x'));
    }
}
