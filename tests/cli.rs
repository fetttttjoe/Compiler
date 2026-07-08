use std::process::{Command, Output};

fn compiler(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .args(args)
        .output()
        .expect("failed to run compiler binary")
}

/// A per-process scratch directory for tests that write their own programs.
fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ys-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// A consumer closing the pipe must terminate the program (EPIPE), not
/// leave it spinning into a dead pipe.
#[test]
fn closed_stdout_pipe_terminates_the_program() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let dir = tempdir();
    std::fs::write(
        dir.join("spin.ys"),
        "fun main() { var i: int = 0; while true { print(i); i = i + 1; } }",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .arg(dir.join("spin.ys"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Read a little, then close our end of the pipe.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 64];
    let _ = stdout.read(&mut buf).unwrap();
    drop(stdout);
    // The program must exit on its own, quickly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "expected quiet exit on EPIPE: {status:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "program kept running after its consumer closed the pipe"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A full disk (or any non-pipe write failure) must not exit 0 with the
/// output silently lost.
#[test]
fn undeliverable_output_is_not_a_silent_success() {
    use std::process::{Command, Stdio};
    if !std::path::Path::new("/dev/full").exists() {
        return; // linux-only probe
    }
    let dir = tempdir();
    std::fs::write(dir.join("ok.ys"), "fun main(): int { return 42; }").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .arg(dir.join("ok.ys"))
        .stdout(std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "ENOSPC must not look like success");
}
