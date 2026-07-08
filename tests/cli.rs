mod common;
use common::{compiler, compiler_in};

/// A per-process scratch directory for tests that write their own programs.
fn tempdir() -> std::path::PathBuf {
    common::tempdir("ys-cli-test")
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
        .stdout(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        )
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "ENOSPC must not look like success");
}

#[test]
fn build_rejects_unsupported_constructs_with_a_diagnostic() {
    let dir = tempdir();
    // floats and value-struct literals are still beyond codegen; value
    // optionals (int?) are gated because 0 and null would be one bit
    // pattern in the word-sized model.
    std::fs::write(
        dir.join("uncompilable.ys"),
        "fun main(): int { var f: float = 1.5; f = f * 2.0; return 1; }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("uncompilable.ys").to_str().unwrap(),
        "-o",
        dir.join("uncompilable").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not yet compilable"), "{err}");
}

/// The end-to-end milestone: the multi-module fib example compiles to a
/// native binary whose exit code is fib(10) = 55.
#[test]
fn builds_the_multi_module_fib_example() {
    let dir = tempdir();
    let bin = dir.join("fib_example");
    let out = compiler(&["build", "examples/main.ys", "-o", bin.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::process::Command::new(&bin)
        .output()
        .expect("failed to run built binary");
    assert_eq!(run.status.code(), Some(55));
}

/// A parameterized main would read argc/argv as its "arguments" once
/// compiled (the interpreter has no arguments to pass either) — both
/// engines must refuse it identically, up front.
#[test]
fn main_with_parameters_is_rejected_by_both_engines() {
    let dir = tempdir();
    std::fs::write(
        dir.join("mainargs.ys"),
        "fun main(a: int): int { return a; }",
    )
    .unwrap();
    let src = dir.join("mainargs.ys");
    let bin = dir.join("mainargs");
    let modes: [&[&str]; 2] = [
        &[src.to_str().unwrap()],
        &["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()],
    ];
    for args in modes {
        let out = compiler(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("takes no parameters"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Compiled out-of-bounds access aborts (SIGABRT) — the deferred-trap
/// policy, like SIGFPE for the idiv traps: never silent corruption. The
/// interpreter diagnoses the same program cleanly.
#[test]
fn compiled_out_of_bounds_aborts_instead_of_corrupting() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempdir();
    std::fs::write(
        dir.join("oob.ys"),
        "fun main(): int { const xs: int[] = [1, 2]; return xs[5]; }",
    )
    .unwrap();
    let src = dir.join("oob.ys");
    let out = compiler(&[src.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "oracle diagnoses OOB");

    let bin = dir.join("oob");
    let out = compiler(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
    assert!(out.status.success());
    let run = std::process::Command::new(&bin)
        .output()
        .expect("failed to run built binary");
    assert_eq!(run.status.signal(), Some(6), "SIGABRT, not silent reads");
}

/// Value-typed optionals can't compile in the word-sized model (0 and
/// null would share a bit pattern); reference optionals ride free.
#[test]
fn value_optionals_are_not_yet_compilable() {
    let dir = tempdir();
    std::fs::write(
        dir.join("valopt.ys"),
        "fun main(): int { var x: int? = null; if x == null { return 1; } return 0; }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("valopt.ys").to_str().unwrap(),
        "-o",
        dir.join("valopt").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet compilable"));
}

/// A recursive value struct has infinite size; the checker allows the
/// declaration (its values are unconstructible), so codegen must
/// diagnose instead of recursing forever.
#[test]
fn recursive_value_struct_is_a_clean_diagnostic() {
    let dir = tempdir();
    std::fs::write(
        dir.join("recur.ys"),
        "struct S { s: S }
        fun f(p: S): int { return 0; }
        fun main(): int { return 1; }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("recur.ys").to_str().unwrap(),
        "-o",
        dir.join("recur").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet compilable"));
}

/// Arrays of multi-word values need stride machinery ys_push and the
/// indexers don't have yet.
#[test]
fn arrays_of_value_structs_are_not_yet_compilable() {
    let dir = tempdir();
    std::fs::write(
        dir.join("valarr.ys"),
        "struct P { x: int, y: int }
        fun main(): int { const ps: P[] = [P { x: 1, y: 2 }]; return ps[0].x; }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("valarr.ys").to_str().unwrap(),
        "-o",
        dir.join("valarr").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet compilable"));
}

/// Printing aggregates needs the debug renderer the runtime doesn't
/// have yet; scalars and strings print, the rest diagnoses.
#[test]
fn printing_structs_is_not_yet_compilable() {
    let dir = tempdir();
    std::fs::write(
        dir.join("printstruct.ys"),
        "refstruct P { x: int }
        fun main(): int { print(P { x: 1 }); return 0; }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("printstruct.ys").to_str().unwrap(),
        "-o",
        dir.join("printstruct").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet compilable"));
}

#[test]
fn more_than_six_parameters_is_not_yet_compilable() {
    let dir = tempdir();
    std::fs::write(
        dir.join("seven.ys"),
        "fun f(a: int, b: int, c: int, d: int, e: int, g: int, h: int): int { return a; }
        fun main(): int { return f(1, 2, 3, 4, 5, 6, 7); }",
    )
    .unwrap();
    let out = compiler(&[
        "build",
        dir.join("seven.ys").to_str().unwrap(),
        "-o",
        dir.join("seven").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not yet compilable"));
}

#[test]
fn build_refuses_to_overwrite_its_own_source() {
    let dir = tempdir();
    let program = "fun main(): int { return 1; }";
    let src = dir.join("clob");
    std::fs::write(&src, program).unwrap();

    // Extensionless entry: the default output stem IS the source file.
    let out = compiler_in(&dir, &["build", "clob"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overwrite"));

    // An alternate spelling of the same file must not fool the guard.
    let alias = format!("{}/./clob", dir.display());
    let out = compiler_in(&dir, &["build", "clob", "-o", &alias]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overwrite"));

    // Nor a hard link (same inode behind a different path).
    let _ = std::fs::remove_file(dir.join("lnk"));
    std::fs::hard_link(&src, dir.join("lnk")).unwrap();
    let out = compiler_in(&dir, &["build", "clob", "-o", "lnk"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overwrite"));

    assert_eq!(
        std::fs::read_to_string(&src).unwrap(),
        program,
        "source must survive every refused build"
    );
}

#[test]
fn build_never_writes_assembly_over_a_source_file() {
    let dir = tempdir();
    // A source named like an assembly artifact: its default output stems
    // to "prog", whose .s sibling is the source itself.
    let program = "fun main(): int { return 2; }";
    std::fs::write(dir.join("prog.s"), program).unwrap();
    let out = compiler_in(&dir, &["build", "prog.s"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overwrite"));
    assert_eq!(
        std::fs::read_to_string(dir.join("prog.s")).unwrap(),
        program
    );
}

#[test]
fn build_usage_errors() {
    // Entry omitted: must be usage (exit 2), not "cannot read 'build'".
    let out = compiler(&["build"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));

    // -o without a value, two entries, and an unknown flag are usage too.
    let out = compiler(&["build", "x.ys", "-o"]);
    assert_eq!(out.status.code(), Some(2));
    let out = compiler(&["build", "x.ys", "y.ys"]);
    assert_eq!(out.status.code(), Some(2));
    let out = compiler(&["build", "--wat", "x.ys"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn build_flags_work_in_any_order() {
    let dir = tempdir();
    std::fs::write(dir.join("seven.ys"), "fun main(): int { return 7; }").unwrap();
    let out = compiler_in(&dir, &["build", "-o", "lucky", "seven.ys"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::process::Command::new(dir.join("lucky"))
        .output()
        .expect("failed to run built binary");
    assert_eq!(run.status.code(), Some(7));
}

/// A left-associative operator chain parses at constant depth, so it can
/// build an AST far taller than the parser's nesting guard could see. The
/// parser's per-function operator budget must turn it into a diagnostic —
/// not a stack overflow — before any pass walks the tree. (Interpret and
/// build share the path up to the diagnostic, so one invocation covers both.)
#[test]
fn deep_operator_chain_is_a_diagnostic_not_a_crash() {
    let dir = tempdir();
    let terms = vec!["1"; 70_000].join(" + ");
    std::fs::write(
        dir.join("deep.ys"),
        format!("fun main(): int {{ return {terms}; }}"),
    )
    .unwrap();
    let out = compiler(&[dir.join("deep.ys").to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "must exit cleanly, not die on a signal"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("operators"), "{err}");
}

/// The other face of the same crash class: a `&&` chain in a condition
/// reaches the narrowing walkers, which recurse per clause with no budget
/// of their own. The parser bound protects them by construction.
#[test]
fn deep_condition_chain_is_a_diagnostic_not_a_crash() {
    let dir = tempdir();
    let cond = vec!["true"; 40_000].join(" && ");
    std::fs::write(
        dir.join("deepcond.ys"),
        format!("fun main(): int {{ while {cond} {{ return 1; }} return 0; }}"),
    )
    .unwrap();
    let out = compiler(&[dir.join("deepcond.ys").to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "must exit cleanly, not die on a signal"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("operators"));
}

#[test]
fn build_never_writes_assembly_through_a_symlink() {
    let dir = tempdir();
    std::fs::write(dir.join("ok.ys"), "fun main(): int { return 3; }").unwrap();
    std::fs::write(dir.join("victim.txt"), "IMPORTANT DATA").unwrap();
    let _ = std::fs::remove_file(dir.join("point.s"));
    std::os::unix::fs::symlink("victim.txt", dir.join("point.s")).unwrap();
    // The derived assembly path point.s is a symlink to an innocent file.
    let out = compiler_in(&dir, &["build", "ok.ys", "-o", "point"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlink"));
    assert_eq!(
        std::fs::read_to_string(dir.join("victim.txt")).unwrap(),
        "IMPORTANT DATA"
    );
}

#[test]
fn build_output_may_not_collide_with_its_assembly_file() {
    let dir = tempdir();
    std::fs::write(dir.join("ok.ys"), "fun main(): int { return 3; }").unwrap();
    // -o ending in .s: cc's input and output would be the same file.
    let out = compiler_in(&dir, &["build", "ok.ys", "-o", "foo.s"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("collides"));
    assert!(!dir.join("foo.s").exists(), "nothing may be written");
}

#[test]
fn double_dash_allows_dashed_source_names() {
    let dir = tempdir();
    std::fs::write(dir.join("-o.ys"), "fun main(): int { return 5; }").unwrap();
    let out = compiler_in(&dir, &["build", "-o", "dashout", "--", "-o.ys"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::process::Command::new(dir.join("dashout"))
        .output()
        .expect("failed to run built binary");
    assert_eq!(run.status.code(), Some(5));
}
