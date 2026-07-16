use crate::span::LineIndex;

/// One registered source file: its name, text, global base offset, and
/// per-file line table.
pub struct SourceFile {
    name: String,
    text: String,
    base: usize,
    lines: LineIndex,
}

impl SourceFile {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn base(&self) -> usize {
        self.base
    }
}

/// All source files of a compilation, sharing one global offset space —
/// any `Span` resolves to its file/line/column here (rustc's approach).
/// Spans stay plain `{start, end}` and no downstream phase learns about
/// files.
pub struct SourceMap {
    files: Vec<SourceFile>,
}

/// A resolved location: everything a diagnostic needs to point at source.
pub struct Resolved<'a> {
    pub file: &'a str,
    pub line: usize,
    pub col: usize,
    pub line_text: &'a str,
}

impl SourceMap {
    pub fn new() -> Self {
        SourceMap { files: Vec::new() }
    }

    /// Registers a file and returns its base offset. Bases are assigned
    /// end-to-end with a +1 gap so EOF spans never collide across files.
    pub fn add(&mut self, name: impl Into<String>, text: impl Into<String>) -> usize {
        let text = text.into();
        let base = self
            .files
            .last()
            .map(|f| f.base + f.text.len() + 1)
            .unwrap_or(0);
        self.files.push(SourceFile {
            name: name.into(),
            lines: LineIndex::new(&text),
            base,
            text,
        });
        base
    }

    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    /// Resolves a global offset to its file, 1-based line/column, and the
    /// text of that line.
    pub fn resolve(&self, offset: usize) -> Resolved<'_> {
        let idx = self
            .files
            .partition_point(|f| f.base <= offset)
            .saturating_sub(1);
        let file = &self.files[idx];
        let local = (offset - file.base).min(file.text.len());
        let (line, col) = file.lines.locate(local);
        Resolved {
            file: &file.name,
            line,
            col,
            line_text: file.lines.line_text(&file.text, line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_assigns_bases_with_a_gap() {
        let mut map = SourceMap::new();
        let a = map.add("a.ys", "ab");
        let b = map.add("b.ys", "xyz");
        assert_eq!(a, 0);
        // +1 gap: a zero-width span at a.ys's EOF (offset 2) must not
        // collide with b.ys's first byte.
        assert_eq!(b, 3);
    }

    #[test]
    fn resolve_finds_the_right_file_line_and_column() {
        let mut map = SourceMap::new();
        map.add("a.ys", "one\ntwo");
        let base = map.add("b.ys", "alpha\nbeta");

        let r = map.resolve(5); // 'w' in "two"
        assert_eq!((r.file, r.line, r.col), ("a.ys", 2, 2));
        assert_eq!(r.line_text, "two");

        let r = map.resolve(base + 7); // 'e' in "beta"
        assert_eq!((r.file, r.line, r.col), ("b.ys", 2, 2));
        assert_eq!(r.line_text, "beta");
    }

    #[test]
    fn resolve_handles_eof_offsets() {
        let mut map = SourceMap::new();
        map.add("a.ys", "ab");
        map.add("b.ys", "x");
        // a.ys's EOF sentinel offset resolves into a.ys, not b.ys.
        let r = map.resolve(2);
        assert_eq!((r.file, r.line, r.col), ("a.ys", 1, 3));
    }
}
