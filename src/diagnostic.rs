use crate::span::{LineIndex, Span};
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
    ///  --> 3:15
    ///   |
    /// 3 |     const x = s + 1;
    ///   |               ^^^^^
    /// ```
    ///
    /// Spans reaching past the line are clamped to it; the underline padding
    /// mirrors the line's own tabs so carets stay aligned in a terminal.
    pub fn render(&self, source: &str, index: &LineIndex) -> String {
        let label = match self.severity {
            Severity::Error => "error",
        };
        let (line, col) = index.locate(self.span.start);
        let text = index.line_text(source, line);
        let line_start = self.span.start - (col - 1);
        let line_end = line_start + text.len();

        let pad: String = source[line_start..self.span.start]
            .chars()
            .map(|c| if c == syntax::TAB { syntax::TAB } else { syntax::SPACE })
            .collect();
        let underline_end = self.span.end.clamp(self.span.start, line_end.max(self.span.start));
        let carets = "^".repeat(
            source[self.span.start..underline_end]
                .chars()
                .count()
                .max(1),
        );

        let num = line.to_string();
        let gutter = " ".repeat(num.len());
        format!(
            "{label}: {msg}\n{gutter}--> {line}:{col}\n{gutter} |\n{num} | {text}\n{gutter} | {pad}{carets}",
            msg = self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_source_line_with_caret_underline() {
        let source = "fun f()\n  bad + 1;";
        let index = LineIndex::new(source);
        let diag = Diagnostic::error("undefined variable 'bad'", Span::new(10, 13));
        assert_eq!(
            diag.render(source, &index),
            "error: undefined variable 'bad'\n --> 2:3\n  |\n2 |   bad + 1;\n  |   ^^^"
        );
    }

    #[test]
    fn render_mirrors_tabs_so_carets_stay_aligned() {
        let source = "\tx + 1.0;";
        let index = LineIndex::new(source);
        let diag = Diagnostic::error("mismatched type", Span::new(5, 8));
        assert_eq!(
            diag.render(source, &index),
            "error: mismatched type\n --> 1:6\n  |\n1 | \tx + 1.0;\n  | \t    ^^^"
        );
    }

    #[test]
    fn render_handles_crlf_lines_and_end_of_input_spans() {
        let source = "a\r\nbb";
        let index = LineIndex::new(source);

        let diag = Diagnostic::error("bad", Span::new(3, 5));
        assert_eq!(
            diag.render(source, &index),
            "error: bad\n --> 2:1\n  |\n2 | bb\n  | ^^"
        );

        // A zero-width span at end of input still gets one caret.
        let eof = Diagnostic::error("unexpected end of input", Span::new(5, 5));
        assert_eq!(
            eof.render(source, &index),
            "error: unexpected end of input\n --> 2:3\n  |\n2 | bb\n  |   ^"
        );
    }
}
