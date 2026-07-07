use std::process::{Command, Output};

fn compiler(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .args(args)
        .output()
        .expect("failed to run compiler binary")
}

#[test]
fn runs_a_program_from_its_entry_file() {
    // examples/main.ys imports fib from examples/math.ys — discovery loads it.
    let out = compiler(&["examples/main.ys"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "=> Int(55)");
}

#[test]
fn more_than_one_argument_is_a_usage_error() {
    let out = compiler(&["examples/main.ys", "examples/math.ys"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}

#[test]
fn import_cycles_fail_with_the_cycle_path() {
    let out = compiler(&["tests/fixtures/cycle_a.ys"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("import cycle"), "{err}");
    assert!(err.contains("cycle_a.ys"), "{err}");
    assert!(err.contains("cycle_b.ys"), "{err}");
}

#[test]
fn importing_a_private_item_fails() {
    let out = compiler(&["tests/fixtures/uses_private.ys"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not exported"), "{err}");
}

#[test]
fn reports_errors_with_file_locations_and_is_deterministic() {
    let first = compiler(&["tests/fixtures/broken.ys"]);
    let second = compiler(&["tests/fixtures/broken.ys"]);
    assert_eq!(first.status.code(), Some(1));
    let err = String::from_utf8_lossy(&first.stderr);
    assert!(err.contains("--> tests/fixtures/broken.ys:2:12"), "{err}");
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
    let out = compiler(&["no/such/file.ys"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}
