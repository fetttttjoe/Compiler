use crate::span::{LineIndex, Span};

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

    /// Renders as `error: <message> at <line>:<col>`.
    pub fn render(&self, index: &LineIndex) -> String {
        let (line, col) = index.locate(self.span.start);
        let label = match self.severity {
            Severity::Error => "error",
        };
        format!("{label}: {} at {line}:{col}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_message_and_location() {
        let source = "fun f()\n  bad";
        let index = LineIndex::new(source);
        let diag = Diagnostic::error("unexpected token", Span::new(10, 13));
        assert_eq!(diag.render(&index), "error: unexpected token at 2:3");
    }
}
