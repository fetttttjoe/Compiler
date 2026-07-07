use crate::source::SourceMap;
use crate::span::Span;
use crate::syntax;

/// The ANSI palette for error output, shared by every path that prints an
/// `error:` label (diagnostics here, top-level errors in `main`): bold red
/// severity, bold blue gutter accents, bold cyan help.
pub const ANSI_ERROR: &str = "\x1b[1;31m";
const ANSI_ACCENT: &str = "\x1b[1;34m";
const ANSI_HELP: &str = "\x1b[1;36m";
pub const ANSI_RESET: &str = "\x1b[0m";

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

    /// Attaches a "did you mean 'x'?" help when a close candidate exists.
    pub fn suggest<'a>(
        self,
        name: &str,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        match closest(name, candidates) {
            Some(suggestion) => self.with_help(format!("did you mean '{suggestion}'?")),
            None => self,
        }
    }

    /// Plain-text convenience over `render_styled` — the form the test
    /// suites assert against; production renders via `render_styled`.
    #[cfg(test)]
    pub fn render(&self, map: &SourceMap) -> String {
        self.render_styled(map, false)
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
    ///
    /// When `color` is true the label, gutter, carets, and help are wrapped
    /// in ANSI color; with `color = false` the output is byte-for-byte the
    /// plain form above, so the caller enables color only for a real
    /// terminal (see `main`). ANSI codes are zero-width, so caret alignment
    /// is unaffected either way.
    pub fn render_styled(&self, map: &SourceMap, color: bool) -> String {
        // (severity, accent for gutter/arrow, help, reset). Empty when off.
        let (sev, accent, help_style, reset) = if color {
            (ANSI_ERROR, ANSI_ACCENT, ANSI_HELP, ANSI_RESET)
        } else {
            ("", "", "", "")
        };
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
            "{sev}{label}{reset}: {msg}\n\
             {gutter}{accent}-->{reset} {file}:{line}:{col}\n\
             {gutter} {accent}|{reset}\n\
             {accent}{num}{reset} {accent}|{reset} {text}\n\
             {gutter} {accent}|{reset} {pad}{sev}{carets}{reset}",
            msg = self.message,
            file = loc.file,
            line = loc.line,
            col = loc.col,
            text = loc.line_text,
        );
        if let Some(help) = &self.help {
            rendered.push_str(&format!(
                "\n{gutter} {accent}={reset} {help_style}help:{reset} {help}"
            ));
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
    fn render_styled_colors_with_only_zero_width_codes() {
        let mut map = SourceMap::new();
        map.add("main.ys", "bad");
        let diag = Diagnostic::error("undefined variable 'bad'", Span::new(0, 3))
            .with_help("did you mean 'bat'?");

        let plain = diag.render(&map);
        // color=false is byte-for-byte the plain form the other tests assert.
        assert_eq!(diag.render_styled(&map, false), plain);

        let colored = diag.render_styled(&map, true);
        // It actually colors: label and carets carry the error (red) code.
        assert!(
            colored.contains(&format!("{ANSI_ERROR}error{ANSI_RESET}")),
            "{colored:?}"
        );
        assert!(
            colored.contains(&format!("{ANSI_ERROR}^^^{ANSI_RESET}")),
            "{colored:?}"
        );
        // ...and adds nothing but zero-width codes — stripping every ANSI
        // escape (`\x1b...m`) recovers the plain form, so caret alignment is
        // identical in a terminal. Palette-agnostic on purpose: new colors
        // must not require touching this test.
        let mut stripped = String::new();
        let mut rest = colored.as_str();
        while let Some(i) = rest.find('\x1b') {
            stripped.push_str(&rest[..i]);
            let end = rest[i..].find('m').expect("unterminated ANSI escape");
            rest = &rest[i + end + 1..];
        }
        stripped.push_str(rest);
        assert_eq!(stripped, plain);
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
