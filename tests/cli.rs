mod common;
use common::{compiler, compiler_in};

/// Builds `src` and asserts the clean slice-gate diagnostic. The gate
/// set shrinks slice by slice; when it empties, delete this helper.
fn assert_not_yet_compilable(name: &str, src: &str) {
    let dir = tempdir();
    std::fs::write(dir.join(format!("{name}.ys")), src).unwrap();
    let out = compiler(&[
        "build",
        dir.join(format!("{name}.ys")).to_str().unwrap(),
        "-o",
        dir.join(name).to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{name}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not yet compilable"),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A per-process scratch directory for tests that write their own programs.
fn tempdir() -> std::path::PathBuf {
    common::tempdir("ys-cli-test")
}

#[test]
fn main_error_union_exits_one_with_the_trap_shape() {
    // ADR 0034 decision 8: an error escaping main(): int! prints
    // `error: error.Name` on stderr and exits 1 — no result line —
    // in BOTH engines.
    let dir = tempdir();
    std::fs::write(
        dir.join("m.ys"),
        "error Nope;\n\
         fun main(): int! {\n\
             print(\"hi\");\n\
             if 2 > 1 { return error.Nope; }\n\
             return 0;\n\
         }",
    )
    .unwrap();
    let src = dir.join("m.ys");
    let out = compiler(&[src.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("error: error.Nope"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bin = dir.join("m");
    let out = compiler(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = std::process::Command::new(&bin).output().unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "hi\n");
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("error: error.Nope"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
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
fn trailing_cli_arguments_reach_the_program() {
    // ADR 0031: everything after the entry file belongs to the program
    // (the old "more than one argument" usage error is gone).
    let dir = tempdir();
    std::fs::write(
        dir.join("args.ys"),
        "fun main(args: string[]): int {
            for a in args { print(a); }
            return len(args);
        }",
    )
    .unwrap();
    let src = dir.join("args.ys");
    let out = compiler(&[src.to_str().unwrap(), "alpha", "beta"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "alpha\nbeta\n=> Int(2)\n"
    );
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
fn ir_without_entry_is_a_usage_error() {
    let out = compiler(&["ir"]);
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

#[test]
fn ir_command_is_deterministic_and_writes_no_artifacts() {
    let dir = tempdir();
    let src = dir.join("ir_dump.ys");
    let asm = dir.join("ir_dump.s");
    let bin = dir.join("ir_dump");
    let _ = std::fs::remove_file(&asm);
    let _ = std::fs::remove_file(&bin);
    std::fs::write(&src, "fun main(): int { return 40 + 2; }").unwrap();

    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_Compiler"))
            .current_dir(&dir)
            .args(["ir", "ir_dump.ys"])
            .env("PATH", "")
            .output()
            .expect("failed to run compiler")
    };
    let first = run();
    let second = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        String::from_utf8_lossy(&first.stdout),
        "fn main [module 0] (params 0, vregs 3) {\n\
         \x20\x20v0 = const 40\n\
         \x20\x20v1 = add.imm v0, 2\n\
         \x20\x20ret v1\n\
         \x20\x20v2 = const 0\n\
         \x20\x20ret v2\n\
         }\n"
    );
    assert!(!asm.exists(), "IR inspection must not write assembly");
    assert!(!bin.exists(), "IR inspection must not write a binary");
}

#[test]
fn tree_showcase_runs_in_both_engines() {
    const PRINTED: &str = "tree total\n37\ntree minimum\n1\n";

    let interpreted = compiler(&["examples/tree/main.ys"]);
    assert!(
        interpreted.status.success(),
        "{}",
        String::from_utf8_lossy(&interpreted.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&interpreted.stdout),
        format!("{PRINTED}=> Int(37)\n")
    );

    let dir = tempdir();
    let bin = dir.join("tree_showcase");
    let built = compiler(&[
        "build",
        "examples/tree/main.ys",
        "-o",
        bin.to_str().unwrap(),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = std::process::Command::new(&bin)
        .output()
        .expect("failed to run tree showcase");
    assert_eq!(run.status.code(), Some(37));
    assert_eq!(String::from_utf8_lossy(&run.stdout), PRINTED);
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

/// Compiled runtime errors report the oracle's message plus a source
/// location and exit 1 (ADR 0022) — never a signal, never silent
/// corruption. Both engines agree on exit code and core message.
#[test]
fn compiled_runtime_errors_report_and_exit_1() {
    let dir = tempdir();
    let scratch = dir.join("rt_io.txt");
    let p = scratch.to_str().unwrap();
    let cases: [(&str, String, &str); 7] = [
        (
            "rt_f2i",
            "fun main(): int { return int(0.0 / 0.0); }".to_string(),
            "invalid float to int conversion",
        ),
        (
            "rt_closed",
            format!(
                "fun main(): int {{\n    const f: file? = open(\"{p}\", \"w\");\n    if f != null {{ close(f); write(f, \"x\"); }}\n    return 0;\n}}"
            ),
            "operation on closed file",
        ),
        (
            "rt_readsize",
            format!(
                "fun main(): int {{\n    const f: file? = open(\"{p}\", \"w\");\n    if f != null {{ read(f, 0); }}\n    return 0;\n}}"
            ),
            "read size must be positive",
        ),
        (
            "rt_closed_read",
            // Closed outranks the size check — both engines agree.
            format!(
                "fun main(): int {{\n    const f: file? = open(\"{p}\", \"w\");\n    if f != null {{ close(f); read(f, 0); }}\n    return 0;\n}}"
            ),
            "operation on closed file",
        ),
        (
            "rt_oob",
            "fun main(): int { const xs: int[] = [1, 2]; return xs[5]; }".to_string(),
            "index 5 out of bounds (length 2)",
        ),
        (
            "rt_div0",
            "fun main(): int { var z: int = 0; return 7 / z; }".to_string(),
            "division by zero",
        ),
        (
            "rt_ovf",
            "fun main(): int {\n    var m: int = -9223372036854775807 - 1;\n    var d: int = -1;\n    return m % d;\n}"
                .to_string(),
            "division overflow",
        ),
    ];
    for (name, program, message) in cases {
        let src = dir.join(format!("{name}.ys"));
        std::fs::write(&src, program).unwrap();
        let out = compiler(&[src.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(1), "{name}: oracle exits 1");
        let oracle_err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(oracle_err.contains(message), "{name}: oracle message");
        // The compiled binary's entire stderr is the oracle's first two
        // lines — message and location — byte-identical (ADR 0022).
        let head: String = oracle_err
            .lines()
            .take(2)
            .map(|l| format!("{l}\n"))
            .collect();

        let bin = dir.join(name);
        let out = compiler(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let run = std::process::Command::new(&bin)
            .output()
            .expect("failed to run built binary");
        assert_eq!(
            run.status.code(),
            Some(1),
            "{name}: exit code, not a signal"
        );
        assert_eq!(
            String::from_utf8_lossy(&run.stderr),
            head,
            "{name}: stderr must be the oracle's first two lines"
        );
        let err = String::from_utf8_lossy(&run.stderr);
        assert!(
            err.contains(&format!("{name}.ys:")),
            "{name}: location: {err}"
        );
    }
}

/// A recursive value struct has infinite size; the checker allows the
/// declaration (its values are unconstructible), so codegen must
/// diagnose instead of recursing forever.
#[test]
fn recursive_value_struct_is_a_clean_diagnostic() {
    assert_not_yet_compilable(
        "recur",
        "struct S { s: S }
        fun f(p: S): int { return 0; }
        fun main(): int { return 1; }",
    );
    // string() of the same unconstructible type reports, never falls back.
    assert_not_yet_compilable(
        "recurstr",
        "struct S { s: S }
        fun f(p: S): string { return string(p); }
        fun main(): int { return 1; }",
    );
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
