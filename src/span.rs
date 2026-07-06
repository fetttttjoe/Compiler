/// A half-open byte range `[start, end)` into the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

/// Precomputed line-start byte offsets, used to turn a byte offset into a
/// human-readable line:column only when rendering a diagnostic.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// Returns (line, column), both 1-based.
    pub fn locate(&self, offset: usize) -> (usize, usize) {
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        (line + 1, offset - self.line_starts[line] + 1)
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
}
