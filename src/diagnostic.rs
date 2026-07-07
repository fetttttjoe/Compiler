use crate::source::SourceMap;
use crate::span::Span;
use crate::syntax;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
        }
    }

    /// Renders the diagnostic with its source line and a caret underline:
    ///
    /// ```text
    /// error: cannot apply '+' to string and int
    ///  --> src/main.lang:3:15
    ///   |
    /// 3 |     const x = s + 1;
    ///   |               ^^^^^
    /// ```
    ///
    /// Spans reaching past the line are clamped to it; the underline padding
    /// mirrors the line's own tabs so carets stay aligned in a terminal.
    pub fn render(&self, map: &SourceMap) -> String {
        let label = match self.severity {
            Severity::Error => "error",
        };
        let loc = map.resolve(self.span.start);

        // Everything the underline needs is inside the resolved line:
        // `col - 1` is the byte offset of the span within it.
        let in_line = loc.col - 1;
        let pad: String = loc.line_text[..in_line]
            .chars()
            .map(|c| if c == syntax::TAB { syntax::TAB } else { syntax::SPACE })
            .collect();
        // saturating: a span must never be able to panic the error reporter.
        let span_len = (self.span.end - self.span.start)
            .min(loc.line_text.len().saturating_sub(in_line));
        let carets = "^".repeat(
            loc.line_text[in_line..in_line + span_len]
                .chars()
                .count()
                .max(1),
        );

        let num = loc.line.to_string();
        let gutter = " ".repeat(num.len());
        format!(
            "{label}: {msg}\n{gutter}--> {file}:{line}:{col}\n{gutter} |\n{num} | {text}\n{gutter} | {pad}{carets}",
            msg = self.message,
            file = loc.file,
            line = loc.line,
            col = loc.col,
            text = loc.line_text,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_file_line_and_caret_underline() {
        let mut map = SourceMap::new();
        map.add("main.lang", "fun f()\n  bad + 1;");
        let diag = Diagnostic::error("undefined variable 'bad'", Span::new(10, 13));
        assert_eq!(
            diag.render(&map),
            "error: undefined variable 'bad'\n --> main.lang:2:3\n  |\n2 |   bad + 1;\n  |   ^^^"
        );
    }

    #[test]
    fn render_points_into_the_second_file() {
        let mut map = SourceMap::new();
        map.add("a.lang", "one");
        let base = map.add("b.lang", "\tx + 1.0;");
        let diag = Diagnostic::error("mismatched type", Span::new(base + 5, base + 8));
        assert_eq!(
            diag.render(&map),
            "error: mismatched type\n --> b.lang:1:6\n  |\n1 | \tx + 1.0;\n  | \t    ^^^"
        );
    }

    #[test]
    fn render_clamps_eof_spans_to_one_caret() {
        let mut map = SourceMap::new();
        map.add("a.lang", "a\r\nbb");
        let eof = Diagnostic::error("unexpected end of input", Span::new(5, 5));
        assert_eq!(
            eof.render(&map),
            "error: unexpected end of input\n --> a.lang:2:3\n  |\n2 | bb\n  |   ^"
        );
    }
}
