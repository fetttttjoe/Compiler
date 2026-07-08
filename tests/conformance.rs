//! The conformance corpus (ADR 0017): `conformance/*.ys` with golden
//! oracle output in `*.out`. Every engine must reproduce the goldens —
//! the interpreter byte-for-byte, the compiled binary matching the print
//! lines and the result value as its exit code. The corpus, not any
//! engine, is the portable definition of the language; features land
//! with corpus files in the same commit.

mod common;
use common::{compiler, tempdir};

#[test]
fn every_engine_reproduces_the_corpus() {
    let dir = tempdir("ys-conformance");
    let mut checked = 0;
    for entry in std::fs::read_dir("conformance").expect("conformance/ exists") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "ys") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let golden = std::fs::read_to_string(path.with_extension("out"))
            .unwrap_or_else(|_| panic!("{name}: golden .out missing"));

        // The interpreter is the normative semantics: byte-for-byte.
        let out = compiler(&[path.to_str().unwrap()]);
        assert!(out.status.success(), "{name}: oracle failed");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            golden,
            "{name}: interpreter diverged from its golden"
        );

        // The compiled engine: same prints, result value as exit code.
        let trimmed = golden.trim_end_matches('\n');
        let (prints, result) = match trimmed.rsplit_once('\n') {
            Some((p, r)) => (format!("{p}\n"), r),
            None => (String::new(), trimmed),
        };
        let value: i64 = result
            .strip_prefix("=> Int(")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{name}: golden must end with an int result"))
            .parse()
            .unwrap();
        let bin = dir.join(&name);
        let out = compiler(&["build", path.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
        assert!(
            out.status.success(),
            "{name}: build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let run = std::process::Command::new(&bin).output().unwrap();
        assert_eq!(
            run.status.code(),
            Some((value & 0xff) as i32),
            "{name}: compiled exit code diverged"
        );
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            prints,
            "{name}: compiled print output diverged"
        );
        checked += 1;
    }
    assert!(checked >= 8, "corpus unexpectedly small: {checked}");
}
