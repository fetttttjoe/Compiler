//! Differential tests: every compiled program runs through both engines
//! and the native exit code must match the interpreter's value. Per ADR
//! 0009 this harness IS the test suite for the backend.

mod common;
use common::{compiler, tempdir};
use std::process::Command;

/// Runs `program` through the interpreter (the oracle) and the native
/// build, then asserts the binary's exit code equals the interpreted
/// value masked to 8 bits (Unix truncates exit codes to one byte).
fn diff(name: &str, program: &str) {
    let dir = tempdir("ys-diff-test");
    let src = dir.join(format!("{name}.ys"));
    std::fs::write(&src, program).unwrap();

    let out = compiler(&[src.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: i64 = stdout
        .trim()
        .strip_prefix("=> Int(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("oracle printed a non-int: {stdout}"))
        .parse()
        .unwrap();

    let bin = dir.join(name);
    let out = compiler(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&bin)
        .output()
        .expect("failed to run built binary");
    assert_eq!(
        run.status.code(),
        Some((value & 0xff) as i32),
        "'{name}' diverged from oracle value {value}"
    );
}

#[test]
fn literal() {
    diff("literal", "fun main(): int { return 42; }");
}

#[test]
fn precedence() {
    diff("precedence", "fun main(): int { return 2 + 3 * 4 - 5; }");
}

#[test]
fn nested_parens_exercise_the_operand_stack() {
    diff(
        "nested",
        "fun main(): int { return ((1 + 2) * (3 + 4) + (5 - 6)) * (2 + (8 % 3)); }",
    );
}

#[test]
fn negative_result_wraps_to_high_exit_code() {
    diff("neg", "fun main(): int { return -1; }");
}

#[test]
fn value_above_one_byte_is_masked() {
    diff("big", "fun main(): int { return 300; }");
}

#[test]
fn division_truncates_toward_zero() {
    diff("div", "fun main(): int { return -7 / 2; }");
}

#[test]
fn remainder_keeps_the_dividend_sign() {
    diff("rem", "fun main(): int { return -7 % 2; }");
}

#[test]
fn literal_beyond_i32_range() {
    diff(
        "wide",
        "fun main(): int { return 123456789012345 % 1000 + 7; }",
    );
}

#[test]
fn locals_and_assignment() {
    diff(
        "locals",
        "fun main(): int { var x: int = 5; x = x + 37; return x; }",
    );
}

#[test]
fn locals_depend_on_each_other() {
    diff(
        "deps",
        "fun main(): int {
            const a: int = 6;
            const b: int = a * 7;
            var c: int = b - a;
            c = c + a % 4;
            return c * 2 - b;
        }",
    );
}

#[test]
fn many_locals_fill_many_slots() {
    // 20 slots exercise frame sizing past one 16-byte alignment unit.
    let decls: String = (0..20)
        .map(|i| format!("var x{i}: int = {i} * 3;"))
        .collect();
    let sum = (0..20)
        .map(|i| format!("x{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    diff(
        "slots",
        &format!("fun main(): int {{ {decls} return {sum}; }}"),
    );
}

#[test]
fn assignment_reads_the_old_value() {
    diff(
        "swapish",
        "fun main(): int {
            var a: int = 3;
            var b: int = 10;
            a = b - a;
            b = b - a;
            return a * 100 + b;
        }",
    );
}

#[test]
fn long_operator_chain_within_the_depth_budget() {
    // Left-associative chains parse at constant depth but build an AST as
    // tall as the chain is long; 6000 terms used to overflow the checker's
    // stack. Within the budget they must compile and agree with the oracle.
    let terms = vec!["1"; 6000].join(" + ");
    diff("chain", &format!("fun main(): int {{ return {terms}; }}"));
}
