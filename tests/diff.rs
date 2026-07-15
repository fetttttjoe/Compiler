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
    diff_io(name, program, &[], b"");
}

/// Spawns with piped stdio and feeds `stdin` — both engines must see
/// the same world (ADR 0031); an empty pipe is a deterministic EOF.
fn run_with(cmd: &mut Command, stdin: &[u8]) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().expect("wait")
}

/// `diff` with program arguments and stdin. Output comparison is on
/// RAW BYTES — strings are bytes (ADR 0013), and lossy re-decoding
/// would hide exactly the divergences the harness exists to catch.
fn diff_io(name: &str, program: &str, args: &[&str], stdin: &[u8]) {
    let dir = tempdir("ys-diff-test");
    let src = dir.join(format!("{name}.ys"));
    std::fs::write(&src, program).unwrap();

    let mut oracle = Command::new(env!("CARGO_BIN_EXE_Compiler"));
    oracle.arg(src.to_str().unwrap()).args(args);
    let out = run_with(&mut oracle, stdin);
    assert!(
        out.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The oracle's stdout is the program's print output plus a final
    // "=> value" result line; the compiled binary must reproduce the
    // print output exactly and the value as its exit code.
    let stdout = out.stdout;
    assert_eq!(stdout.last(), Some(&b'\n'), "oracle output unterminated");
    let cut = stdout[..stdout.len() - 1].iter().rposition(|&b| b == b'\n');
    let (prints, result) = match cut {
        Some(i) => (&stdout[..i + 1], &stdout[i + 1..stdout.len() - 1]),
        None => (&stdout[..0], &stdout[..stdout.len() - 1]),
    };
    let value: i64 = std::str::from_utf8(result)
        .ok()
        .and_then(|r| r.strip_prefix("=> Int("))
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("oracle printed a non-int result"))
        .parse()
        .unwrap();

    let bin = dir.join(name);
    let out = compiler(&["build", src.to_str().unwrap(), "-o", bin.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut native = Command::new(&bin);
    native.args(args);
    let run = run_with(&mut native, stdin);
    assert_eq!(
        run.status.code(),
        Some((value & 0xff) as i32),
        "'{name}' diverged from oracle value {value}"
    );
    assert_eq!(
        run.stdout, prints,
        "'{name}' print output diverged from the oracle"
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
fn power_of_two_division_at_the_integer_extremes() {
    // The strength-reduced shift sequences must truncate toward zero
    // exactly like idiv, i64::MIN included.
    diff(
        "pow2edge",
        "fun main(): int {
            const min: int = -9223372036854775807 - 1;
            var r: int = 0;
            if min / 2 == -4611686018427387904 { r = r + 1; }
            if min % 2 == 0 { r = r + 10; }
            if -9 / 4 == -2 { r = r + 100; }
            if -9 % 4 == -1 { r = r + 1000; }
            return r % 251;
        }",
    );
}

#[test]
fn constant_division_across_divisors_and_extremes() {
    // Magic-multiply strength reduction must agree with idiv for every
    // divisor shape (odd, even, large) across the dividend extremes.
    diff(
        "magicdiv",
        "fun probe(x: int): int {
            var acc: int = x / 3 + x % 3;
            acc = acc + x / 7 - x % 7;
            acc = acc + x / 10 + x % 10;
            acc = acc + x / 12 - x % 12;
            acc = acc + x / 1000 + x % 1000;
            acc = acc + x / 249 - x % 249;
            return acc;
        }
        fun main(): int {
            const xs: int[] = [0, 1, -1, 2, -2, 6, -6, 7, -7, 41, -41,
                100, -100, 123456789, -987654321,
                9223372036854775807, -9223372036854775807 - 1];
            var acc: int = 0;
            for x in xs {
                acc = acc + probe(x);
            }
            return acc % 251;
        }",
    );
}

#[test]
fn huge_power_of_two_divisors_still_compile() {
    // 2^k divisors with k > 31 can't ride the leaq-bias shift sequence
    // (32-bit displacement limit); they must take the magic path.
    diff(
        "hugepow2",
        "fun main(): int {
            var x: int = 987654321987;
            var r: int = (x / 4294967296) % 251;
            r = r + (x % 4294967296) % 251;
            r = r + ((0 - x) / 4611686018427387904) % 251;
            r = r + ((0 - x) % 4294967296) % 251;
            return r % 251;
        }",
    );
}

#[test]
fn for_in_over_an_empty_array_never_runs() {
    diff(
        "emptyiter",
        "fun main(): int {
            var xs: int[] = [];
            var hits: int = 0;
            for x in xs { hits = hits + x + 100; }
            return hits + len(xs) + 9;
        }",
    );
}

#[test]
fn min_int_through_the_magic_path_for_huge_divisors() {
    // 2^32 skips the shift sequence (k > 31) and must take the magic
    // path even at the i64 extremes.
    diff(
        "minhuge",
        "fun main(): int {
            const min: int = -9223372036854775807 - 1;
            var r: int = (min / 4294967296) % 251;
            r = r + (min % 4294967296) % 251;
            return r % 251;
        }",
    );
}

#[test]
fn string_concat_reassigned_in_a_loop() {
    diff(
        "concatloop",
        "fun main(): int {
            var s: string = \"x\";
            var i: int = 0;
            while i < 5 {
                s = s + \"y\" + \"z\";
                i = i + 1;
            }
            print(s);
            if s == \"xyzyzyzyzyz\" { return 1; }
            return 0;
        }",
    );
}

#[test]
fn runtime_divisors_go_through_the_checked_path() {
    // ADR 0022 adds zero/MIN÷-1 checks before idiv; clean divisions
    // must be bit-identical to the oracle across the extremes.
    diff(
        "checkeddiv",
        "fun div(a: int, b: int): int { return a / b + a % b; }
        fun main(): int {
            const xs: int[] = [1, -1, 2, -2, 3, 7, 10, 100, 251,
                9223372036854775807, -9223372036854775807 - 1];
            var acc: int = 0;
            for a in xs {
                for b in xs {
                    if a == -9223372036854775807 - 1 && b == -1 {
                        acc = acc + 1;
                    } else {
                        acc = acc + div(a, b) % 251;
                    }
                }
            }
            return acc % 251;
        }",
    );
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
fn if_else_branches() {
    diff(
        "ifelse",
        "fun main(): int {
            var r: int = 0;
            if 2 < 3 { r = 10; } else { r = 20; }
            if 5 <= 4 { r = r + 100; } else { r = r + 1; }
            if r == 11 { return r * 2; }
            return 0;
        }",
    );
}

#[test]
fn while_loop_accumulates() {
    diff(
        "sum",
        "fun main(): int {
            var i: int = 1;
            var sum: int = 0;
            while i <= 10 {
                sum = sum + i;
                i = i + 1;
            }
            return sum;
        }",
    );
}

#[test]
fn diverging_guards_narrow_and_compile_to_unchecked_loads() {
    // ADR 0020: after `if cur == null {...}` guards, reads lower without
    // null checks — the compiled path must agree with the oracle.
    diff(
        "guards",
        "refstruct Node { v: int, next: Node? }
        fun build(n: int): Node? {
            var head: Node? = null;
            var i: int = 0;
            while i < n {
                i = i + 1;
                head = Node { v: i, next: head };
            }
            return head;
        }
        fun sum_skipping_evens(head: Node?): int {
            var cur: Node? = head;
            var acc: int = 0;
            while cur != null {
                if cur.v % 2 == 0 { cur = cur.next; continue; }
                acc = acc + cur.v;
                cur = cur.next;
            }
            return acc;
        }
        fun find(head: Node?, needle: int): int {
            var cur: Node? = head;
            while true {
                if cur == null { break; }
                if cur.v == needle { return cur.v * 10; }
                cur = cur.next;
            }
            return -1;
        }
        fun first_or_zero(head: Node?): int {
            if head == null { return 0; }
            return head.v;
        }
        fun main(): int {
            const list: Node? = build(9);
            print(sum_skipping_evens(list));
            print(find(list, 4));
            print(find(list, 99));
            print(first_or_zero(list));
            print(first_or_zero(null));
            return sum_skipping_evens(list) + first_or_zero(list);
        }",
    );
}

#[test]
fn else_divergence_carries_the_survivors() {
    diff(
        "elsediverge",
        "refstruct Box { v: int }
        fun pick(b: Box?): int {
            if b != null { print(b.v); } else { return -5; }
            return b.v * 2;
        }
        fun main(): int {
            print(pick(Box { v: 21 }));
            print(pick(null));
            return pick(Box { v: 50 });
        }",
    );
}

#[test]
fn value_optionals_wrap_narrow_and_compare() {
    diff(
        "valopt",
        "fun grade(score: int?): int {
            if score == null { return -1; }
            return score * 10;
        }
        fun main(): int {
            var x: int? = null;
            var r: int = 0;
            if x == null { r = r + 1; }
            x = 42;
            if x != null { r = r + x; }
            r = r + grade(x) / 10 + grade(null);
            print(x);
            x = null;
            print(x);
            return r;
        }",
    );
}

#[test]
fn optional_equality_matrix_and_coalesce() {
    diff(
        "opteq",
        "fun main(): int {
            var a: int? = 5;
            var b: int? = 5;
            var c: int? = null;
            var r: int = 0;
            if a == b { r = r + 1; }
            if a == 5 { r = r + 2; }
            if 5 == a { r = r + 4; }
            if a == c { r = r + 8; }
            if c == null { r = r + 16; }
            if null == c { r = r + 32; }
            if a != c { r = r + 64; }
            r = r + (c ?? 7) + (a ?? 100);
            var d: int? = c ?? a;
            r = r + (d ?? 1000);
            return r;
        }",
    );
}

#[test]
fn float_optionals_use_ieee_payload_equality() {
    diff(
        "floatopt",
        "fun main(): int {
            var f: float? = 2.5;
            var g: float? = null;
            var r: int = 0;
            if f != null {
                if f * 2.0 == 5.0 { r = r + 1; }
            }
            if f == 2.5 { r = r + 2; }
            if g == null { r = r + 4; }
            if f == g { r = r + 8; }
            if (g ?? 1.5) == 1.5 { r = r + 16; }
            return r;
        }",
    );
}

#[test]
fn string_optionals_carry_content() {
    diff(
        "stropt",
        "fun pick(s: string?, d: string): string {
            return s ?? d;
        }
        fun main(): int {
            var s: string? = \"hi\";
            var t: string? = null;
            var r: int = 0;
            if s == \"hi\" { r = r + 1; }
            if s != null {
                if s + \"!\" == \"hi!\" { r = r + 2; }
            }
            if t == null { r = r + 4; }
            if s == t { r = r + 8; }
            print(pick(s, \"fallback\"));
            print(pick(t, \"fallback\"));
            print(s);
            print(t);
            return r;
        }",
    );
}

#[test]
fn value_struct_optionals_wrap_and_compare() {
    diff(
        "structopt",
        "struct P { x: int, y: int }
        fun shift(p: P?): P {
            if p == null { return P { x: 0, y: 0 }; }
            return P { x: p.x + 1, y: p.y };
        }
        fun main(): int {
            var p: P? = P { x: 1, y: 2 };
            var q: P? = null;
            var r: int = 0;
            if p != null { r = r + p.x + p.y; }
            if p == (P { x: 1, y: 2 }) { r = r + 4; }
            if q == null { r = r + 8; }
            if p != q { r = r + 16; }
            const s: P = shift(p);
            r = r + s.x * 100;
            const t: P = shift(null);
            r = r + t.x + t.y;
            return r;
        }",
    );
}

#[test]
fn optional_chaining_builds_value_optionals() {
    diff(
        "optchainval",
        "refstruct Node { v: int, tag: int?, next: Node? }
        fun main(): int {
            const n: Node = Node { v: 7, tag: 3, next: null };
            var m: Node? = null;
            var r: int = 0;
            r = r + (n.next?.v ?? 100);
            var t: int? = m?.v;
            if t == null { r = r + 1; }
            t = n.next?.tag;
            if t == null { r = r + 2; }
            var u: int? = Node { v: 1, tag: 9, next: null }.tag;
            r = r + (u ?? 50);
            return r;
        }",
    );
}

#[test]
fn optional_fields_inside_structs_stay_canonical() {
    // Struct equality memcmps the whole layout — sound only because
    // every null wrap zeroes its payload (ADR 0021 decision 2).
    diff(
        "optfield",
        "struct Slot { id: int, extra: int? }
        fun main(): int {
            var a: Slot = Slot { id: 1, extra: null };
            var b: Slot = Slot { id: 1, extra: null };
            var r: int = 0;
            if a == b { r = r + 1; }
            a.extra = 5;
            if a != b { r = r + 2; }
            b.extra = 5;
            if a == b { r = r + 4; }
            if a.extra != null { r = r + a.extra; }
            var e: int? = b.extra;
            r = r + (e ?? 100) * 10;
            return r;
        }",
    );
}

#[test]
fn optional_returns_ride_sret() {
    diff(
        "optret",
        "fun find(xs: int[], needle: int): int? {
            for x in xs {
                if x == needle { return x; }
            }
            return null;
        }
        fun main(): int {
            const xs: int[] = [3, 9, 27];
            var r: int = 0;
            r = r + (find(xs, 9) ?? -1);
            r = r + (find(xs, 5) ?? -1) * 10;
            print(find(xs, 27));
            print(find(xs, 4));
            return r;
        }",
    );
}

#[test]
fn break_and_continue_in_while_loops() {
    diff(
        "bcwhile",
        "fun main(): int {
            var acc: int = 0;
            var i: int = 0;
            while i < 100 {
                if i == 7 { break; }
                i = i + 1;
                if i % 3 == 0 { continue; }
                acc = acc + i;
            }
            return acc * 10 + i;
        }",
    );
}

#[test]
fn continue_in_for_still_advances_the_element() {
    diff(
        "bcfor",
        "fun main(): int {
            const xs: int[] = [5, 6, 7, 8, 9];
            var total: int = 0;
            for [i, x] in xs {
                if x % 2 == 0 { continue; }
                total = total + i * 100 + x;
            }
            return total % 251;
        }",
    );
}

#[test]
fn break_stops_a_live_for_that_keeps_growing() {
    // The body pushes every step — only break ends the iteration.
    diff(
        "bclive",
        "fun main(): int {
            var xs: int[] = [1, 2];
            var seen: int = 0;
            for x in xs {
                seen = seen + 1;
                push(xs, x + 10);
                if seen == 5 { break; }
            }
            return seen * 20 + len(xs) + xs[6];
        }",
    );
}

#[test]
fn break_and_continue_bind_to_the_innermost_loop() {
    diff(
        "bcnested",
        "fun main(): int {
            var log: int = 0;
            var i: int = 0;
            while i < 4 {
                i = i + 1;
                for x in [1, 2, 3] {
                    if x == 2 { continue; }
                    if i == 3 { break; }
                    log = log + x;
                }
                if i == 4 { break; }
                log = log * 2;
            }
            print(log);
            return log % 251;
        }",
    );
}

#[test]
fn comparisons_and_logic() {
    diff(
        "cmp",
        "fun main(): int {
            var r: int = 0;
            const t: bool = 3 > 2;
            const f: bool = 3 != 3;
            if t && !f { r = r + 1; }
            if t || f { r = r + 2; }
            if f && t { r = r + 4; }
            if 1 >= 1 { r = r + 8; }
            return r;
        }",
    );
}

#[test]
fn logic_short_circuits_past_a_trap() {
    // The right side would divide by zero; the oracle short-circuits, so
    // the compiled code must never evaluate it either.
    diff(
        "shortcircuit",
        "fun main(): int {
            const zero: int = 0;
            if 1 == 2 && 1 / zero == 0 { return 1; }
            if 2 == 2 || 1 / zero == 0 { return 42; }
            return 0;
        }",
    );
}

#[test]
fn block_scoped_locals_shadow_and_restore() {
    diff(
        "blocks",
        "fun main(): int {
            var x: int = 1;
            if x == 1 {
                var x: int = 50;
                x = x + 1;
            }
            var i: int = 0;
            while i < 2 {
                const step: int = 3;
                x = x + step;
                i = i + 1;
            }
            return x;
        }",
    );
}

#[test]
fn nested_control_flow() {
    diff(
        "nested_cf",
        "fun main(): int {
            var total: int = 0;
            var i: int = 0;
            while i < 5 {
                var j: int = 0;
                while j < 5 {
                    if (i + j) % 2 == 0 { total = total + i * j; }
                    j = j + 1;
                }
                i = i + 1;
            }
            return total;
        }",
    );
}

#[test]
fn simple_call_with_argument_order() {
    // Subtraction makes swapped arguments observable.
    diff(
        "callorder",
        "fun sub(a: int, b: int): int { return a - b; }
        fun main(): int { return sub(50, 8); }",
    );
}

#[test]
fn recursion_computes_fib() {
    diff(
        "fib",
        "fun fib(n: int): int {
            if n < 2 { return n; }
            return fib(n - 1) + fib(n - 2);
        }
        fun main(): int { return fib(10); }",
    );
}

#[test]
fn six_arguments_fill_every_register() {
    diff(
        "sixargs",
        "fun mix(a: int, b: int, c: int, d: int, e: int, f: int): int {
            return a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6;
        }
        fun main(): int { return mix(1, 2, 3, 4, 5, 6); }",
    );
}

#[test]
fn call_inside_expression_operands() {
    // A call while the operand stack holds a pending left side exercises
    // the 16-byte alignment fix-up at the call site.
    diff(
        "callinexpr",
        "fun three(): int { return 3; }
        fun main(): int { return 1 + three() * 2 + (5 - three()); }",
    );
}

#[test]
fn bool_params_and_returns() {
    diff(
        "boolfn",
        "fun even(n: int): bool { return n % 2 == 0; }
        fun pick(flag: bool, a: int, b: int): int {
            if flag { return a; }
            return b;
        }
        fun main(): int { return pick(even(10), 7, 9) * pick(even(3), 100, 11); }",
    );
}

#[test]
fn unit_function_called_as_a_statement() {
    diff(
        "unitcall",
        "fun noop() { return; }
        fun main(): int { noop(); return 5; }",
    );
}

#[test]
fn array_literals_and_indexing() {
    diff(
        "arrays",
        "fun main(): int {
            const xs: int[] = [10, 20, 30, 40];
            var sum: int = xs[0] + xs[3];
            xs[1] = xs[1] * 2;
            sum = sum + xs[1];
            return sum;
        }",
    );
}

#[test]
fn push_grows_and_len_tracks() {
    diff(
        "push",
        "fun main(): int {
            var xs: int[] = [];
            var i: int = 0;
            while i < 100 {
                push(xs, i * 2);
                i = i + 1;
            }
            return len(xs) + xs[99];
        }",
    );
}

#[test]
fn for_in_iterates_with_index_binding() {
    diff(
        "forin",
        "fun main(): int {
            const xs: int[] = [5, 6, 7, 8, 9];
            var total: int = 0;
            for [i, x] in xs {
                total = total + i * x;
            }
            for x in xs {
                total = total + x;
            }
            return total;
        }",
    );
}

#[test]
fn for_in_is_live_when_the_body_pushes() {
    // The oracle re-reads the length each step; pushing inside the body
    // extends the iteration.
    diff(
        "liveiter",
        "fun main(): int {
            var xs: int[] = [1, 2];
            var seen: int = 0;
            for x in xs {
                seen = seen + x;
                if len(xs) < 6 { push(xs, x * 10); }
            }
            return seen % 251;
        }",
    );
}

#[test]
fn arrays_alias_through_handles() {
    diff(
        "alias",
        "fun bump(a: int[]) { a[0] = a[0] + 100; }
        fun main(): int {
            const xs: int[] = [7];
            const ys: int[] = xs;
            bump(ys);
            return xs[0];
        }",
    );
}

#[test]
fn nested_arrays_of_handles() {
    diff(
        "nested_arr",
        "fun main(): int {
            const grid: int[][] = [[1, 2], [3, 4, 5]];
            push(grid[0], 9);
            return grid[0][2] * 10 + grid[1][1] + len(grid[0]);
        }",
    );
}

#[test]
fn array_returned_from_a_function() {
    diff(
        "arr_ret",
        "fun range(n: int): int[] {
            var xs: int[] = [];
            var i: int = 0;
            while i < n { push(xs, i); i = i + 1; }
            return xs;
        }
        fun main(): int {
            var total: int = 0;
            for x in range(20) { total = total + x; }
            return total;
        }",
    );
}

#[test]
fn refstruct_fields_and_aliasing() {
    diff(
        "refstruct",
        "refstruct Counter { n: int, step: int }
        fun bump(c: Counter) { c.n = c.n + c.step; }
        fun main(): int {
            const a: Counter = Counter { n: 5, step: 10 };
            const b: Counter = a;
            bump(b);
            bump(a);
            return a.n;
        }",
    );
}

#[test]
fn linked_list_with_optionals() {
    diff(
        "list",
        "refstruct Node { value: int, next: Node? }
        fun main(): int {
            var head: Node? = null;
            var i: int = 1;
            while i <= 10 {
                head = Node { value: i, next: head };
                i = i + 1;
            }
            var sum: int = 0;
            var cur: Node? = head;
            while cur != null {
                sum = sum + cur.value;
                cur = cur.next;
            }
            return sum;
        }",
    );
}

#[test]
fn optional_chaining_and_coalescing() {
    diff(
        "optchain",
        "refstruct Node { value: int, next: Node? }
        fun main(): int {
            const tail: Node = Node { value: 30, next: null };
            const head: Node = Node { value: 12, next: tail };
            const a: Node? = head.next?.next;
            var r: int = 0;
            if a == null { r = r + 1; }
            const b: Node? = head.next ?? head;
            if b != null { r = r + b.value * 10; }
            return r;
        }",
    );
}

#[test]
fn refstruct_identity_equality() {
    diff(
        "identity",
        "refstruct P { x: int }
        fun main(): int {
            const a: P = P { x: 1 };
            const b: P = a;
            const c: P = P { x: 1 };
            var r: int = 0;
            if a == b { r = r + 1; }
            if a == c { r = r + 10; }
            if a != c { r = r + 100; }
            return r;
        }",
    );
}

#[test]
fn refstructs_inside_arrays() {
    diff(
        "refarr",
        "refstruct Box { v: int }
        fun main(): int {
            const boxes: Box[] = [Box { v: 1 }, Box { v: 2 }];
            push(boxes, Box { v: 3 });
            boxes[1].v = 20;
            var total: int = 0;
            for b in boxes {
                total = total + b.v;
            }
            return total;
        }",
    );
}

#[test]
fn nested_field_chains() {
    diff(
        "chains",
        "refstruct Inner { v: int }
        refstruct Outer { inner: Inner, tag: int }
        fun main(): int {
            const o: Outer = Outer { inner: Inner { v: 7 }, tag: 3 };
            o.inner.v = o.inner.v * o.tag;
            return o.inner.v;
        }",
    );
}

#[test]
fn value_structs_copy_on_assignment() {
    diff(
        "valstruct",
        "struct Point { x: int, y: int }
        fun main(): int {
            const a: Point = Point { x: 3, y: 4 };
            var b: Point = a;
            b.x = 30;
            return a.x * 1000 + b.x + a.y;
        }",
    );
}

#[test]
fn value_structs_through_calls_and_returns() {
    diff(
        "valcalls",
        "struct Point { x: int, y: int }
        fun make(x: int, y: int): Point {
            return Point { x: x, y: y };
        }
        fun taxi(p: Point): int { return p.x + p.y; }
        fun main(): int {
            const p: Point = make(20, 22);
            return taxi(p) + taxi(make(1, 2)) * 100;
        }",
    );
}

#[test]
fn nested_value_structs_inline() {
    diff(
        "valnest",
        "struct Inner { v: int, w: int }
        struct Outer { pre: int, inner: Inner, post: int }
        fun main(): int {
            var o: Outer = Outer { pre: 1, inner: Inner { v: 2, w: 3 }, post: 4 };
            var i: Inner = o.inner;
            i.v = 20;
            o.inner.w = 30;
            return o.pre + o.inner.v * 10 + o.inner.w + i.v * 100 + o.post;
        }",
    );
}

#[test]
fn value_struct_equality_is_structural() {
    diff(
        "valeq",
        "struct Pair { a: int, b: bool }
        fun main(): int {
            const x: Pair = Pair { a: 5, b: true };
            const y: Pair = Pair { a: 5, b: true };
            const z: Pair = Pair { a: 5, b: false };
            var r: int = 0;
            if x == y { r = r + 1; }
            if x == z { r = r + 10; }
            if x != z { r = r + 100; }
            return r;
        }",
    );
}

#[test]
fn value_struct_inside_a_refstruct() {
    diff(
        "valinref",
        "struct Pos { x: int, y: int }
        refstruct Entity { pos: Pos, hp: int }
        fun main(): int {
            const e: Entity = Entity { pos: Pos { x: 1, y: 2 }, hp: 100 };
            var copy: Pos = e.pos;
            copy.x = 50;
            e.pos.y = 20;
            e.pos = Pos { x: e.pos.x + 6, y: e.pos.y + 1 };
            return e.pos.x + e.pos.y * 10 + copy.x * 100 + e.hp;
        }",
    );
}

#[test]
fn struct_with_refstruct_field_compares_by_identity() {
    diff(
        "mixedeq",
        "refstruct Shared { n: int }
        struct Tag { label: int, shared: Shared }
        fun main(): int {
            const s: Shared = Shared { n: 1 };
            const a: Tag = Tag { label: 7, shared: s };
            const b: Tag = Tag { label: 7, shared: s };
            const c: Tag = Tag { label: 7, shared: Shared { n: 1 } };
            var r: int = 0;
            if a == b { r = r + 1; }
            if a == c { r = r + 10; }
            return r;
        }",
    );
}

#[test]
fn print_scalars_and_strings() {
    diff(
        "print",
        "fun main(): int {
            print(42);
            print(-7);
            print(true);
            print(1 > 2);
            print(\"hello, world\");
            print(\"\");
            var i: int = 0;
            while i < 3 { print(i * 10); i = i + 1; }
            return 5;
        }",
    );
}

#[test]
fn strings_copy_pass_and_return() {
    diff(
        "strings",
        "fun pick(flag: bool, a: string, b: string): string {
            if flag { return a; }
            return b;
        }
        fun main(): int {
            const greet: string = \"héllo\";
            var s: string = greet;
            print(pick(true, s, \"other\"));
            print(pick(false, s, \"other\"));
            return 3;
        }",
    );
}

#[test]
fn string_equality_is_content_equality() {
    diff(
        "streq",
        "fun exclaim(): string { return \"yo!\"; }
        fun main(): int {
            const a: string = \"yo!\";
            var r: int = 0;
            if a == exclaim() { r = r + 1; }
            if a == \"yo\" { r = r + 10; }
            if a != \"no\" { r = r + 100; }
            if \"\" == \"\" { r = r + 1000; }
            return r;
        }",
    );
}

#[test]
fn strings_inside_structs() {
    diff(
        "strfield",
        "refstruct Named { name: string, id: int }
        struct Tag { label: string }
        fun main(): int {
            const n: Named = Named { name: \"widget\", id: 7 };
            print(n.name);
            var t: Tag = Tag { label: \"a\" };
            t.label = \"b\";
            print(t.label);
            if t.label == \"b\" { return n.id; }
            return 0;
        }",
    );
}

#[test]
fn string_concatenation_allocates() {
    diff(
        "concat",
        "fun main(): int {
            const a: string = \"foo\";
            var s: string = a + \"bar\";
            s = s + \"!\";
            print(s);
            print(\"x\" + \"y\" + \"z\");
            print(\"\" + \"\");
            if a + \"bar\" == \"foobar\" { return 7; }
            return 0;
        }",
    );
}

#[test]
fn struct_params_copy_at_the_call() {
    // The callee mutates the underlying storage through an alias; the
    // param must have been copied at argument evaluation, like the oracle.
    diff(
        "paramcopy",
        "struct Pos { x: int, y: int }
        refstruct Entity { pos: Pos, hp: int }
        fun f(p: Pos, e: Entity): int { e.pos.x = 99; return p.x; }
        fun main(): int {
            const e: Entity = Entity { pos: Pos { x: 1, y: 2 }, hp: 0 };
            return f(e.pos, e) * 100 + e.pos.x;
        }",
    );
}

#[test]
fn equality_snapshots_its_left_operand() {
    // The right side mutates the left side's storage during evaluation;
    // the oracle compares the value from before.
    diff(
        "eqsnapshot",
        "struct Pos { x: int, y: int }
        refstruct Entity { pos: Pos, hp: int }
        fun clobber(e: Entity): Pos {
            e.pos.x = 77;
            return Pos { x: 1, y: 2 };
        }
        fun main(): int {
            const e: Entity = Entity { pos: Pos { x: 1, y: 2 }, hp: 0 };
            if e.pos == clobber(e) { return 1; }
            return 2;
        }",
    );
}

#[test]
fn single_word_value_structs_are_still_pointers() {
    // Kind::Struct{words:1} must never be mistaken for an inline word.
    diff(
        "onefield",
        "struct V { z: int }
        fun getz(v: V): int { return v.z; }
        fun main(): int {
            const a: V = V { z: 42 };
            const b: int = 999;
            var r: int = 0;
            if a == (V { z: 42 }) { r = r + 1; }
            return a.z + getz(a) + r + b - 999;
        }",
    );
}

#[test]
fn assignment_evaluates_the_value_first() {
    // The oracle evaluates the RHS before the target's base and index;
    // print side effects expose the order.
    diff(
        "assignorder",
        "fun eff(n: int): int { print(n); return n; }
        refstruct B { v: int }
        fun main(): int {
            var xs: int[] = [10, 20, 30];
            xs[eff(1)] = eff(2) + eff(3);
            const bs: B[] = [B { v: 7 }];
            bs[eff(0)].v = eff(5);
            return xs[1] + bs[0].v;
        }",
    );
}

#[test]
fn float_arithmetic_and_comparisons() {
    diff(
        "floats",
        "fun main(): int {
            const a: float = 2.5;
            var b: float = a * 4.0 - 1.5;
            b = b / 2.0 + 0.25;
            var r: int = 0;
            if b == 4.5 { r = r + 1; }
            if a < b { r = r + 10; }
            if b <= 4.5 { r = r + 100; }
            return r;
        }",
    );
}

#[test]
fn float_ieee_edge_cases() {
    diff(
        "ieee",
        "fun main(): int {
            const zero: float = 0.0;
            const nan: float = zero / zero;
            const inf: float = 1.0 / zero;
            var r: int = 0;
            if nan == nan { r = r + 1; }
            if nan != nan { r = r + 2; }
            if nan < 1.0 { r = r + 4; }
            if nan >= 1.0 { r = r + 8; }
            if -0.0 == 0.0 { r = r + 16; }
            if inf > 1000000.0 { r = r + 32; }
            if -inf < inf { r = r + 64; }
            return r;
        }",
    );
}

#[test]
fn float_remainder_matches_fmod() {
    diff(
        "fmod",
        "fun main(): int {
            var r: int = 0;
            if 7.5 % 2.0 == 1.5 { r = r + 1; }
            if -7.5 % 2.0 == -1.5 { r = r + 10; }
            if 7.5 % -2.0 == 1.5 { r = r + 100; }
            return r;
        }",
    );
}

#[test]
fn floats_through_calls_arrays_and_fields() {
    diff(
        "floatflow",
        "refstruct Point { x: float, y: float }
        fun dist2(p: Point): float { return p.x * p.x + p.y * p.y; }
        fun main(): int {
            const p: Point = Point { x: 3.0, y: 4.0 };
            var samples: float[] = [0.5, 1.5];
            push(samples, 2.5);
            var sum: float = 0.0;
            for s in samples {
                sum = sum + s;
            }
            var r: int = 0;
            if dist2(p) == 25.0 { r = r + 1; }
            if sum == 4.5 { r = r + 10; }
            if -p.x == 0.0 - 3.0 { r = r + 100; }
            return r;
        }",
    );
}

// ---- Multi-word array elements (ADR 0023) -------------------------------

#[test]
fn value_struct_arrays_literal_index_assign_push_and_for() {
    diff(
        "vsarr",
        "struct P { x: int, y: int }
        fun main(): int {
            var ps: P[] = [P { x: 1, y: 2 }, P { x: 3, y: 4 }];
            print(ps[0].x + ps[1].y * 10);
            push(ps, P { x: 5, y: 6 });
            ps[0] = ps[2];
            var sum: int = 0;
            for p in ps { sum = sum + p.x * 10 + p.y; }
            print(sum);
            return len(ps);
        }",
    );
}

#[test]
fn push_of_own_element_survives_every_realloc() {
    // push(xs, xs[0]) snapshots at evaluation — the realloc push itself
    // triggers must not invalidate the source pointer.
    diff(
        "pushself",
        "struct P { x: int, y: int }
        fun main(): int {
            var ps: P[] = [P { x: 7, y: 8 }];
            push(ps, ps[0]);
            push(ps, ps[1]);
            push(ps, ps[2]);
            push(ps, ps[3]);
            return ps[4].x * 10 + ps[4].y;
        }",
    );
}

#[test]
fn index_assign_value_snapshots_before_the_target_pushes() {
    // The value evaluates (and copies) before the target's index
    // expression pushes and moves the buffer.
    diff(
        "idxsnap",
        "struct P { x: int }
        fun grow(ps: P[]): int {
            push(ps, P { x: 99 });
            return 0;
        }
        fun main(): int {
            var ps: P[] = [P { x: 1 }, P { x: 2 }];
            ps[grow(ps)] = ps[1];
            return ps[0].x + len(ps);
        }",
    );
}

#[test]
fn element_equality_snapshots_before_rhs_mutation() {
    // xs[0] == f(xs) where f rewrites xs[0]: the left value compares as
    // it was BEFORE the call, the oracle's order.
    diff(
        "eqsnap",
        "struct P { x: int }
        fun clobber(ps: P[]): P {
            ps[0] = P { x: 99 };
            return P { x: 1 };
        }
        fun main(): int {
            var ps: P[] = [P { x: 1 }];
            if ps[0] == clobber(ps) { return 1; }
            return 0;
        }",
    );
}

#[test]
fn for_over_struct_elements_reads_length_live() {
    diff(
        "vslive",
        "struct P { x: int }
        fun main(): int {
            var ps: P[] = [P { x: 1 }, P { x: 2 }];
            var n: int = 0;
            for p in ps {
                if len(ps) < 4 { push(ps, P { x: p.x * 10 }); }
                n = n + p.x;
            }
            return n;
        }",
    );
}

#[test]
fn string_arrays_literal_push_assign_compare_and_print() {
    diff(
        "strarr",
        r#"fun main(): int {
            var ss: string[] = ["a", "bb"];
            push(ss, ss[0] + ss[1]);
            ss[0] = "z";
            for s in ss { print(s); }
            print(ss[2] == "abb");
            print(ss[0] == ss[1]);
            return len(ss);
        }"#,
    );
}

#[test]
fn struct_with_string_field_array_elements() {
    diff(
        "strfarr",
        r#"struct Tag { name: string, id: int }
        fun main(): int {
            var ts: Tag[] = [Tag { name: "a", id: 1 }];
            push(ts, Tag { name: "b" + "c", id: 2 });
            for t in ts { print(t.name); }
            print(ts[1].name == "bc");
            return ts[0].id + ts[1].id;
        }"#,
    );
}

#[test]
fn nested_value_struct_elements_and_field_writes_through_them() {
    diff(
        "nestarr",
        "struct In { a: int, b: int }
        struct Out { p: In, q: int }
        fun main(): int {
            var os: Out[] = [Out { p: In { a: 1, b: 2 }, q: 3 }];
            push(os, os[0]);
            os[1].q = 9;
            os[0].p.b = 8;
            const x: In = os[1].p;
            return x.a * 1000 + x.b * 100 + os[0].p.b * 10 + os[1].q;
        }",
    );
}

#[test]
fn pushing_a_struct_returning_call() {
    diff(
        "pushsret",
        "struct P { x: int, y: int }
        fun mk(a: int): P { return P { x: a, y: a + 1 }; }
        fun main(): int {
            var ps: P[] = [mk(1)];
            push(ps, mk(3));
            return ps[1].y * 10 + ps[0].x;
        }",
    );
}

#[test]
fn optional_int_arrays_wrap_narrow_compare_and_coalesce() {
    diff(
        "optarr",
        "fun main(): int {
            var xs: int?[] = [1, null, 3];
            push(xs, null);
            push(xs, 5);
            var sum: int = 0;
            for x in xs {
                if x != null { sum = sum + x; }
                sum = sum + (x ?? 100);
            }
            print(xs[1] == null);
            print(xs[0] == xs[2]);
            print(xs[2] == 3);
            xs[0] = null;
            print(xs[0] == null);
            return sum;
        }",
    );
}

#[test]
fn optional_element_arrays_through_outer_optionals_and_fields() {
    // The old gate's every-route cases (ADR 0021), now positive.
    diff(
        "optroutes",
        "refstruct S { xs: int?[] }
        fun main(): int {
            var xs: int?[]? = [];
            const s: S = S { xs: [7, null] };
            push(s.xs, 9);
            var n: int = 0;
            for x in s.xs { n = n + (x ?? 1000); }
            if xs != null { n = n + len(xs); }
            return n;
        }",
    );
}

#[test]
fn null_tests_on_no_memcmp_optional_payloads_are_tag_tests() {
    // ADR 0021 decision 5: `x == null` never compares the payload, so
    // it is legal even when the payload class can't memcmp (floats,
    // strings). Payload comparisons (`V? == V`) stay gated — pinned by
    // cli.rs value_optional_gates_are_precise.
    diff(
        "optnulltag",
        "struct V { f: float, s: string }
        fun main(): int {
            var a: V? = null;
            var r: int = 0;
            if a == null { r = r + 1; }
            if null == a { r = r + 10; }
            a = V { f: 1.5, s: \"x\" };
            if a != null { r = r + 100; }
            const vs: V?[] = [null];
            if vs[0] == null { r = r + 1000; }
            return r;
        }",
    );
}

// ---- Stack argument passing (ADR 0024) -----------------------------------

#[test]
fn seven_params_hit_the_first_stack_slot() {
    // Distinct coefficients catch any slot permutation.
    diff(
        "seven",
        "fun f(a: int, b: int, c: int, d: int, e: int, g: int, h: int): int {
            return a + b * 2 + c * 4 + d * 8 + e * 16 + g * 32 + h * 64;
        }
        fun main(): int { return f(1, 1, 1, 1, 1, 1, 1); }",
    );
}

#[test]
fn ten_params_with_strings_and_structs_in_stack_slots() {
    diff(
        "tenparams",
        r#"struct Pair { a: int, b: int }
        fun f(a: int, b: int, c: int, d: int, e: int, g: int, h: int, i: string, j: Pair, k: int): int {
            print(i);
            return a + b + c + d + e + g + h * 10 + j.a + j.b + k * 100;
        }
        fun main(): int {
            return f(1, 2, 3, 4, 5, 6, 7, "stack", Pair { a: 10, b: 20 }, 1);
        }"#,
    );
}

#[test]
fn sret_plus_six_args_spills_the_last_slot() {
    // The hidden destination pointer occupies slot 0, pushing the sixth
    // argument onto the stack.
    diff(
        "sretseven",
        "struct Pair { a: int, b: int }
        fun mk(a: int, b: int, c: int, d: int, e: int, f: int): Pair {
            return Pair { a: a + c + e, b: b + d + f * 100 };
        }
        fun main(): int {
            const p: Pair = mk(1, 2, 3, 4, 5, 6);
            return p.a + p.b;
        }",
    );
}

#[test]
fn stack_args_evaluate_left_to_right() {
    diff(
        "argorder",
        "fun loud(x: int): int { print(x); return x; }
        fun f(a: int, b: int, c: int, d: int, e: int, g: int, h: int, i: int): int {
            return h * 10 + i;
        }
        fun main(): int {
            return f(loud(1), loud(2), loud(3), loud(4), loud(5), loud(6), loud(7), loud(8));
        }",
    );
}

#[test]
fn wide_params_and_wide_calls_in_one_function() {
    // Call-crossing stack params must survive the callee's own wide
    // calls (callee-saved or spilled, never the outgoing area).
    diff(
        "widboth",
        "fun leaf(a: int, b: int, c: int, d: int, e: int, g: int, h: int): int {
            return h - g;
        }
        fun mid(a: int, b: int, c: int, d: int, e: int, g: int, h: int): int {
            const first: int = leaf(h, g, e, d, c, b, a);
            const second: int = leaf(a, b, c, d, e, g, h);
            return first * 100 + second + h * 7;
        }
        fun main(): int { return mid(1, 2, 3, 4, 5, 6, 7); }",
    );
}

#[test]
fn calls_of_different_widths_share_the_outgoing_maximum() {
    diff(
        "maxout",
        "fun seven(a: int, b: int, c: int, d: int, e: int, g: int, h: int): int { return h; }
        fun nine(a: int, b: int, c: int, d: int, e: int, g: int, h: int, i: int, j: int): int {
            return h + i * 10 + j * 100;
        }
        fun main(): int {
            return seven(0, 0, 0, 0, 0, 0, 1) + nine(0, 0, 0, 0, 0, 0, 2, 3, 4);
        }",
    );
}

#[test]
fn float_params_in_stack_slots_move_bitwise() {
    diff(
        "floatslot",
        "fun pick(a: float, b: float, c: float, d: float, e: float, f: float, g: float, h: float): float {
            return g + h;
        }
        fun main(): int {
            if pick(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.25, 2.25) == 3.5 { return 1; }
            return 0;
        }",
    );
}

#[test]
fn eight_param_recursion_stresses_repeated_stack_stores() {
    diff(
        "recur8",
        "fun fib8(n: int, p1: int, p2: int, p3: int, p4: int, p5: int, p6: int, p7: int): int {
            if n < 2 { return n + p1 + p2 + p3 + p4 + p5 + p6 + p7; }
            return fib8(n - 1, p1, p2, p3, p4, p5, p6, p7)
                 + fib8(n - 2, p1, p2, p3, p4, p5, p6, p7);
        }
        fun main(): int { return fib8(8, 1, 0, 1, 0, 1, 0, 1) % 200; }",
    );
}

// ---- Aggregate printing (ADR 0025) ---------------------------------------

#[test]
fn struct_printing_sorts_fields_by_name() {
    // Declaration order {z, a} — the render order is the oracle's
    // name-sorted storage order, not the layout order.
    diff(
        "printsort",
        "struct P { z: int, a: int }
        fun main(): int { print(P { z: 1, a: 2 }); return 0; }",
    );
}

#[test]
fn aggregate_printing_covers_every_shape() {
    diff(
        "printshapes",
        r#"struct P { z: int, a: int }
        struct Tag { name: string, id: int }
        refstruct R { p: P, label: string }
        fun main(): int {
            print([1, 2, 3]);
            print(["a", "bb"]);
            print([P { z: 1, a: 2 }, P { z: 3, a: 4 }]);
            print(Tag { name: "x", id: 7 });
            print(R { p: P { z: 5, a: 6 }, label: "boxed" });
            print([[1], [], [2, 3]]);
            var empty: int[] = [];
            print(empty);
            return 0;
        }"#,
    );
}

#[test]
fn cyclic_refstructs_print_to_the_depth_budget() {
    // The hop costs a level (render.rs): parity needs the SAME budget
    // and the same "..." cutoffs.
    diff(
        "printcycle",
        "refstruct Node { v: int, next: Node? }
        fun main(): int {
            var a: Node = Node { v: 1, next: null };
            var b: Node = Node { v: 2, next: a };
            a.next = b;
            print(b);
            print(a);
            return 0;
        }",
    );
}

#[test]
fn deep_array_nesting_hits_the_depth_floor() {
    diff(
        "printdeep",
        "fun main(): int {
            const xs: int[][][][][][][][][] = [[[[[[[[[7]]]]]]]]];
            print(xs);
            return 0;
        }",
    );
}

#[test]
fn optional_aggregates_print_null_or_the_value() {
    diff(
        "printopt",
        "struct P { z: int, a: int }
        refstruct R { x: int }
        fun main(): int {
            var mp: P? = null;
            print(mp);
            mp = P { z: 9, a: 8 };
            print(mp);
            var mr: R? = null;
            print(mr);
            mr = R { x: 5 };
            print(mr);
            print([1, null, 3]);
            var ss: string?[] = [null];
            push(ss, \"s\");
            print(ss);
            return 0;
        }",
    );
}

#[test]
fn unit_prints_after_its_side_effects() {
    diff(
        "printunit",
        r#"fun noisy() { print("effect"); }
        fun main(): int { print(noisy()); return 0; }"#,
    );
}

#[test]
fn printed_structs_embed_optionals_and_strings() {
    diff(
        "printembed",
        r#"struct S { hit: int?, name: string, flag: bool }
        fun main(): int {
            print(S { hit: null, name: "n1", flag: true });
            print(S { hit: 4, name: "n2", flag: false });
            print([S { hit: 4, name: "n2", flag: false }]);
            return 0;
        }"#,
    );
}

// ---- Structural equality walk (ADR 0026) ---------------------------------

#[test]
fn string_field_structs_compare_by_content() {
    // Different allocations, equal bytes — identity would say no.
    diff(
        "eqstrfield",
        r#"struct Tag { name: string, id: int }
        fun main(): int {
            var r: int = 0;
            const a: Tag = Tag { name: "a" + "x", id: 1 };
            if a == (Tag { name: "ax", id: 1 }) { r = r + 1; }
            if a != (Tag { name: "ay", id: 1 }) { r = r + 10; }
            if a != (Tag { name: "ax", id: 2 }) { r = r + 100; }
            return r;
        }"#,
    );
}

#[test]
fn float_field_structs_compare_ieee() {
    // NaN != NaN and -0.0 == 0.0 — exactly the cases memcmp gets
    // wrong, and exactly the oracle's f64 semantics.
    diff(
        "eqfloatfield",
        "struct V { f: float }
        fun main(): int {
            var r: int = 0;
            const nan: float = 0.0 / 0.0;
            if (V { f: nan }) != (V { f: nan }) { r = r + 1; }
            if (V { f: -0.0 }) == (V { f: 0.0 }) { r = r + 10; }
            if (V { f: 1.5 }) == (V { f: 1.5 }) { r = r + 100; }
            return r;
        }",
    );
}

#[test]
fn nested_no_memcmp_structs_walk_recursively() {
    diff(
        "eqnested",
        r#"struct In { s: string, hit: int? }
        struct Out { v: In, k: int }
        fun main(): int {
            var r: int = 0;
            const a: Out = Out { v: In { s: "x", hit: null }, k: 3 };
            if a == (Out { v: In { s: "x", hit: null }, k: 3 }) { r = r + 1; }
            if a != (Out { v: In { s: "x", hit: 5 }, k: 3 }) { r = r + 10; }
            if a != (Out { v: In { s: "y", hit: null }, k: 3 }) { r = r + 100; }
            return r;
        }"#,
    );
}

#[test]
fn optional_no_memcmp_structs_compare_through_the_tag() {
    // The old gate's cases: V? == V? and V? == V with float/string
    // payloads, plus string? fields inside the walk.
    diff(
        "eqoptwalk",
        r#"struct V { f: float, s: string? }
        fun main(): int {
            var r: int = 0;
            var a: V? = V { f: 1.5, s: "tag" };
            var b: V? = V { f: 1.5, s: "tag" };
            if a == b { r = r + 1; }
            if a == (V { f: 1.5, s: "tag" }) { r = r + 10; }
            b = V { f: 1.5, s: null };
            if a != b { r = r + 100; }
            b = null;
            if a != b { r = r + 1000; }
            if b == null { r = r + 10000; }
            return r;
        }"#,
    );
}

#[test]
fn equality_walk_snapshots_before_rhs_mutation() {
    // s == f() where f rewrites s's string field: the left value
    // compares as it was BEFORE the call.
    diff(
        "eqwalksnap",
        r#"struct Box { s: string }
        refstruct Holder { b: Box }
        fun clobber(h: Holder): Box {
            h.b = Box { s: "changed" };
            return Box { s: "orig" };
        }
        fun main(): int {
            const h: Holder = Holder { b: Box { s: "orig" } };
            if h.b == clobber(h) { return 1; }
            return 0;
        }"#,
    );
}

// ---- Float printing (ADR 0027) -------------------------------------------

/// A value as a float-typed ys literal: its own Display text, with
/// `.0` appended when dot-free (whole numbers would lex as ints).
/// Shortest-round-trip means the lexer reparses the exact bits.
fn float_lit(x: f64) -> String {
    let mut t = format!("{x}");
    if !t.contains('.') {
        t.push_str(".0");
    }
    t
}

#[test]
fn float_display_parity_on_the_fixed_matrix() {
    diff(
        "floatfmt",
        r#"struct P { w: float, name: string }
        fun main(): int {
            print(1.0);
            print(0.1);
            print(-10.5);
            print(-0.0);
            print(1000000000000000000000.0);
            print(0.0000000001);
            print(1.0 / 3.0);
            print(0.1 + 0.2);
            print(0.0 / 0.0);
            print(1.0 / 0.0);
            print(-1.0 / 0.0);
            var m: float? = null;
            print(m);
            m = 1.25;
            print(m);
            print([1.5, -0.25]);
            print(P { w: 0.1, name: "x" });
            return 0;
        }"#,
    );
}

#[test]
fn float_display_parity_at_the_extremes() {
    // Subnormals, the largest finite, 17-digit cases — the zero-run
    // buffer and the precision-16 probe both get exercised.
    let prints: Vec<String> = [
        5e-324,
        f64::MAX,
        f64::MIN_POSITIVE,
        2.225073858507201e-308, // largest subnormal
        0.1 + 0.2,
        9007199254740992.0, // 2^53
        1.23456789012345e300,
        -1e-300,
    ]
    .iter()
    .map(|x| format!("print({});", float_lit(*x)))
    .collect();
    diff(
        "floatedge",
        &format!("fun main(): int {{ {} return 0; }}", prints.join("\n")),
    );
}

#[test]
fn float_display_parity_on_random_bit_patterns() {
    // xorshift64 over the f64 bit space, finite values only. The
    // oracle prints through Rust Display itself, so engine agreement
    // is ground-truth agreement.
    let mut s: u64 = 0x9e3779b97f4a7c15;
    let mut prints = Vec::new();
    while prints.len() < 120 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let x = f64::from_bits(s);
        if !x.is_finite() {
            continue;
        }
        prints.push(format!("print({});", float_lit(x)));
    }
    diff(
        "floatbits",
        &format!("fun main(): int {{ {} return 0; }}", prints.join("\n")),
    );
}

// ---- int↔float conversion (ADR 0028) -------------------------------------

#[test]
fn conversions_round_trip_truncate_and_hit_the_boundaries() {
    diff(
        "convert",
        "fun main(): int {
            print(float(7));
            print(int(1.9));
            print(int(-1.9));
            print(int(-0.0));
            print(float(9007199254740993));
            print(int(-9223372036854775808.0));
            print(int(9223372036854774784.0));
            var sum: float = 0.0;
            var i: int = 1;
            while i <= 10 {
                sum = sum + 1.0 / float(i);
                i = i + 1;
            }
            print(sum);
            return int(sum * 100.0);
        }",
    );
}

#[test]
fn conversions_flow_through_calls_and_aggregates() {
    diff(
        "convflow",
        "struct Stat { avg: float, count: int }
        fun mean(total: int, n: int): float { return float(total) / float(n); }
        fun main(): int {
            const s: Stat = Stat { avg: mean(7, 2), count: 2 };
            print(s);
            const xs: float[] = [0.5, 1.5, 2.5];
            var acc: int = 0;
            for x in xs { acc = acc + int(x * 2.0); }
            return acc + int(s.avg);
        }",
    );
}

// ---- string(x) conversion (ADR 0029) -------------------------------------

#[test]
fn string_conversion_matches_print_for_every_shape() {
    // The defining invariant: print(string(x)) and print(x) write the
    // same bytes — scalars, aggregates, cycles, and optionals alike.
    diff(
        "strconv",
        r#"struct P { x: int, y: float }
        refstruct Node { v: int, next: Node? }
        fun main(): int {
            print(string(42) + "|" + string(true) + "|" + string(false));
            const p: P = P { x: 3, y: 0.5 };
            print(string(p));
            print(p);
            const xs: P[] = [p, P { x: 4, y: 1.5 }];
            print(string(xs));
            print(xs);
            var n: Node = Node { v: 1, next: null };
            n.next = n;
            print(string(n));
            print(n);
            var o: int? = null;
            print(string(o) + "!");
            o = 9;
            if o != null { print(string(o)); }
            return 0;
        }"#,
    );
}

#[test]
fn string_of_floats_round_trips_the_bit_space() {
    // The ADR 0027 harness through the string() path: the same
    // xorshift stream, exercising the builder-and-copy pipeline
    // instead of direct print.
    let mut s: u64 = 0x9e3779b97f4a7c15;
    let mut prints = Vec::new();
    while prints.len() < 120 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let x = f64::from_bits(s);
        if !x.is_finite() {
            continue;
        }
        prints.push(format!("print(string({}));", float_lit(x)));
    }
    diff(
        "strfloatbits",
        &format!("fun main(): int {{ {} return 0; }}", prints.join("\n")),
    );
}

#[test]
fn string_conversion_edges() {
    // An empty str? payload (a zero-length take), builder reuse across
    // a loop, and the float shapes with long zero runs.
    diff(
        "strconvedge",
        r#"fun main(): int {
            var e: string? = "";
            print(string(e) + "|");
            var s: string = "";
            var i: int = 0;
            while i < 5 { s = s + string(i * 1000000000000); i = i + 1; }
            print(s);
            print(string([true, false]) + string(1000000000000000000000.0) + string(0.005));
            return 0;
        }"#,
    );
}

#[test]
fn template_literals_agree_with_manual_concat() {
    // ADR 0030: templates are parser sugar, so both engines see the
    // same desugared concat — nesting, struct literals inside `${}`,
    // escapes, optionals, and loop growth included.
    diff(
        "template",
        r#"struct P { x: int, y: float }
        fun main(): int {
            const name: string = "ada";
            const p: P = P { x: 2, y: 1.5 };
            print(`${name} scored ${p.x * 10} (${p.y})`);
            print(`${name} scored ` + string(p.x * 10) + ` (` + string(p.y) + `)`);
            print(`nested ${`inner ${p.x}`} done`);
            print(`struct ${P { x: 1, y: 0.5 }} end`);
            print(`escaped: \` \${x} $5 b=${1 < 2}`);
            var o: int? = null;
            print(`o=${o}`);
            o = 4;
            var acc: string = "";
            var i: int = 0;
            while i < 3 { acc = `${acc}${i * (o ?? 0)};`; i = i + 1; }
            print(acc);
            return 0;
        }"#,
    );
}

// ---- world interface (ADR 0031) -------------------------------------------

#[test]
fn args_and_stdin_flow_through_both_engines() {
    diff_io(
        "worldargs",
        r#"fun main(args: string[]): int {
            print(`argc=${len(args)}`);
            for a in args { print(`arg=[${a}]`); }
            var n: int = 0;
            var line: string? = readLine();
            while line != null {
                n = n + 1;
                print(`${n}: ${line}`);
                line = readLine();
            }
            return len(args) + n;
        }"#,
        &["alpha", "beta gamma", ""],
        b"first\nsecond\n\nlast without newline",
    );
}

#[test]
fn file_handles_round_trip_bytes() {
    // Both engines run the same program against the same path in
    // sequence; "w" truncates, so each run is deterministic.
    let dir = tempdir("ys-diff-io");
    let path = dir.join("data.txt");
    let p = path.to_str().unwrap();
    diff(
        "worldfile",
        &format!(
            r#"fun main(): int {{
                const w: file? = open("{p}", "w");
                var ok: int = 0;
                if w != null {{
                    if write(w, `x=${{7}}\n`) {{ ok = ok + 1; }}
                    if write(w, "tail") {{ ok = ok + 1; }}
                    if close(w) {{ ok = ok + 1; }}
                }}
                const r: file? = open("{p}", "r");
                if r != null {{
                    print(readLine(r));
                    print(read(r, 2));
                    print(read(r, 99));
                    print(read(r, 99));
                    print(close(r));
                }}
                print(open("{p}/nope", "r") == null);
                print(open("{p}", "rw") == null);
                return ok;
            }}"#
        ),
    );
}

#[test]
fn non_utf8_stdin_bytes_pass_through() {
    // Strings are raw bytes (ADR 0013): invalid UTF-8 must survive the
    // readLine → concat → print pipeline in both engines untouched.
    diff_io(
        "worldbytes",
        r#"fun main(): int {
            var n: int = 0;
            var line: string? = readLine();
            while line != null {
                print("got: " + line);
                n = n + 1;
                line = readLine();
            }
            return n;
        }"#,
        &[],
        b"\xff\xfe raw \x80 bytes\n\xc3(bad continuation\n",
    );
}

#[test]
fn guard_return_narrows_locals_in_both_engines() {
    // ADR 0033: join facts on locals lower to unchecked payload reads —
    // ref-shaped and value optionals alike.
    diff(
        "narrowlocals",
        r#"refstruct P { v: int }
        fun pick(flag: bool): P? {
            if flag { return P { v: 40 }; }
            return null;
        }
        fun main(): int {
            var p: P? = pick(true);
            if p == null { return 1; }
            var n: int? = 2;
            if n == null { return 2; }
            return p.v + n;
        }"#,
    );
}

#[test]
fn guard_return_narrows_a_local_file_handle() {
    // ADR 0033 × ADR 0031: the bind-guard-use resource idiom without
    // else-nesting, through file?, bool, and string? locals.
    let dir = tempdir("ys-narrow-file");
    let path = dir.join("t.txt");
    let p = path.to_str().unwrap();
    diff(
        "narrowfile",
        &format!(
            r#"fun main(): int {{
                const w: file? = open("{p}", "w");
                if w == null {{ return 1; }}
                var ok: int = 0;
                if write(w, "seven=7\n") {{ ok = ok + 1; }}
                if close(w) {{ ok = ok + 1; }}
                const r: file? = open("{p}", "r");
                if r == null {{ return 2; }}
                const line: string? = readLine(r);
                if line == null {{ return 3; }}
                print(line);
                print(close(r));
                return ok;
            }}"#
        ),
    );
}

#[test]
fn error_codes_agree_across_engines() {
    // ADR 0034 slice A: literals, identity equality, rendering through
    // print/string/templates, struct fields, and error? optionals —
    // including narrowing a guarded error? local (ADR 0033).
    diff(
        "errcodes",
        r#"error NotFound, Timeout, Busy;
        struct Req { id: int, fail: error }
        fun main(): int {
            const e: error = error.Timeout;
            print(e);
            print(e == error.Timeout);
            print(e == error.NotFound);
            print(`got ${e}!`);
            const r: Req = Req { id: 7, fail: error.Busy };
            print(r);
            var maybe: error? = null;
            print(maybe);
            maybe = error.NotFound;
            print(maybe);
            if maybe == null { return 0; }
            print(maybe == error.NotFound);
            return 42;
        }"#,
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
