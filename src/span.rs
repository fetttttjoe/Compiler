/// A half-open byte range `[start, end)` into the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// The span covering everything from the start of `self` to the end of `end`.
    pub fn to(self, end: Span) -> Span {
        Span::new(self.start, end.end)
    }
}

/// Precomputed line-start byte offsets, used to turn a byte offset into a
/// human-readable line:column only when rendering a diagnostic.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        LineIndex {
            line_starts: crate::syntax::line_starts(source),
        }
    }

    /// Returns (line, column), both 1-based.
    pub fn locate(&self, offset: usize) -> (usize, usize) {
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        (line + 1, offset - self.line_starts[line] + 1)
    }

    /// The text of a 1-based line, without its trailing line break — used by
    /// diagnostics to show the offending source line.
    pub fn line_text<'s>(&self, source: &'s str, line: usize) -> &'s str {
        let start = self.line_starts[line - 1];
        let end = self.line_starts.get(line).copied().unwrap_or(source.len());
        source[start..end].trim_end_matches([crate::syntax::LF, crate::syntax::CR])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_maps_offsets_to_line_and_column() {
        let index = LineIndex::new("ab\ncde\nf");
        assert_eq!(index.locate(0), (1, 1)); // 'a'
        assert_eq!(index.locate(1), (1, 2)); // 'b'
        assert_eq!(index.locate(3), (2, 1)); // 'c'
        assert_eq!(index.locate(5), (2, 3)); // 'e'
        assert_eq!(index.locate(7), (3, 1)); // 'f'
    }

    #[test]
    fn locate_handles_boundary_offsets() {
        // Empty input and the one-past-the-end offset (EOF diagnostics point
        // there) must resolve without panicking.
        assert_eq!(LineIndex::new("").locate(0), (1, 1));
        let index = LineIndex::new("ab\n");
        assert_eq!(index.locate(3), (2, 1)); // offset == source.len()
    }

    #[test]
    fn locate_handles_crlf_and_cr_line_endings() {
        let crlf = LineIndex::new("ab\r\ncd");
        assert_eq!(crlf.locate(5), (2, 2)); // 'd' on line 2

        let cr = LineIndex::new("ab\rcd");
        assert_eq!(cr.locate(4), (2, 2)); // 'd' on line 2
    }
}
