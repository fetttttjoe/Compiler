use std::process::{Command, Output};

fn compiler(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .args(args)
        .output()
        .expect("failed to run compiler binary")
}

#[test]
fn runs_a_multi_file_program() {
    let out = compiler(&["examples/math.lang", "examples/main.lang"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "=> Int(55)");
}

#[test]
fn reports_errors_with_file_locations_and_is_deterministic() {
    let first = compiler(&["tests/fixtures/broken.lang"]);
    let second = compiler(&["tests/fixtures/broken.lang"]);
    assert_eq!(first.status.code(), Some(1));
    let err = String::from_utf8_lossy(&first.stderr);
    assert!(err.contains("--> tests/fixtures/broken.lang:2:12"), "{err}");
    assert!(err.contains("undefined variable 'undefined'"), "{err}");
    assert_eq!(
        first.stderr, second.stderr,
        "diagnostics must be byte-identical across runs"
    );
}

#[test]
fn no_arguments_is_a_usage_error() {
    let out = compiler(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn unreadable_file_is_a_clean_error() {
    let out = compiler(&["no/such/file.lang"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}
