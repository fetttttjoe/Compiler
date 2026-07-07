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
    /// An optional actionable hint, rendered as a trailing `= help:` line.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
            help: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Renders the diagnostic with its source line and a caret underline:
    ///
    /// ```text
    /// error: cannot apply '+' to string and int
    ///  --> src/main.ys:3:15
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
        let mut rendered = format!(
            "{label}: {msg}\n{gutter}--> {file}:{line}:{col}\n{gutter} |\n{num} | {text}\n{gutter} | {pad}{carets}",
            msg = self.message,
            file = loc.file,
            line = loc.line,
            col = loc.col,
            text = loc.line_text,
        );
        if let Some(help) = &self.help {
            rendered.push_str(&format!("\n{gutter} = help: {help}"));
        }
        rendered
    }
}

/// The closest candidate to `target` — powers "did you mean …?" hints.
/// The allowed edit distance scales with the name's length so short names
/// don't produce absurd suggestions; ties break alphabetically so hints stay
/// deterministic even over hash-map candidates.
pub fn closest<'a>(
    target: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = match target.chars().count() {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    };
    candidates
        .into_iter()
        .map(|c| (edit_distance(target, c), c))
        .filter(|&(d, _)| d > 0 && d <= limit)
        .min_by_key(|&(d, c)| (d, c))
        .map(|(_, c)| c)
}

/// Levenshtein distance (two-row dynamic programming; names are short).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            cur.push(substitution.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_file_line_and_caret_underline() {
        let mut map = SourceMap::new();
        map.add("main.ys", "fun f()\n  bad + 1;");
        let diag = Diagnostic::error("undefined variable 'bad'", Span::new(10, 13));
        assert_eq!(
            diag.render(&map),
            "error: undefined variable 'bad'\n --> main.ys:2:3\n  |\n2 |   bad + 1;\n  |   ^^^"
        );
    }

    #[test]
    fn render_points_into_the_second_file() {
        let mut map = SourceMap::new();
        map.add("a.ys", "one");
        let base = map.add("b.ys", "\tx + 1.0;");
        let diag = Diagnostic::error("mismatched type", Span::new(base + 5, base + 8));
        assert_eq!(
            diag.render(&map),
            "error: mismatched type\n --> b.ys:1:6\n  |\n1 | \tx + 1.0;\n  | \t    ^^^"
        );
    }

    #[test]
    fn render_appends_a_help_line() {
        let mut map = SourceMap::new();
        map.add("main.ys", "bad");
        let diag = Diagnostic::error("undefined variable 'bad'", Span::new(0, 3))
            .with_help("did you mean 'bat'?");
        assert_eq!(
            diag.render(&map),
            "error: undefined variable 'bad'\n --> main.ys:1:1\n  |\n1 | bad\n  | ^^^\n  = help: did you mean 'bat'?"
        );
    }

    #[test]
    fn closest_suggests_only_plausible_names() {
        assert_eq!(closest("acount", ["account", "wildly"]), Some("account"));
        assert_eq!(closest("fibb", ["fib", "fob"]), Some("fib"));
        // Short names never suggest — everything is 1 edit from "x".
        assert_eq!(closest("x", ["y"]), None);
        // Exact matches are not suggestions.
        assert_eq!(closest("fib", ["fib"]), None);
    }

    #[test]
    fn render_clamps_eof_spans_to_one_caret() {
        let mut map = SourceMap::new();
        map.add("a.ys", "a\r\nbb");
        let eof = Diagnostic::error("unexpected end of input", Span::new(5, 5));
        assert_eq!(
            eof.render(&map),
            "error: unexpected end of input\n --> a.ys:2:3\n  |\n2 | bb\n  |   ^"
        );
    }
}
