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
fn long_operator_chain_within_the_depth_budget() {
    // Left-associative chains parse at constant depth but build an AST as
    // tall as the chain is long; 6000 terms used to overflow the checker's
    // stack. Within the budget they must compile and agree with the oracle.
    let terms = vec!["1"; 6000].join(" + ");
    diff("chain", &format!("fun main(): int {{ return {terms}; }}"));
}
