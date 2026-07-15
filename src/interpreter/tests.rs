use super::*;
use crate::modules::{Module, load_program};
use crate::source::SourceMap;
use crate::{check::check, lexer::lex, parser::parse};

fn run(src: &str) -> Result<Value, Diagnostic> {
    run_full(src).map(|(value, _)| value)
}

/// Like `run`, but keeps the heap for rendering assertions.
fn run_full(src: &str) -> Result<(Value, Heap), Diagnostic> {
    let (tokens, ld) = lex(src);
    assert!(ld.is_empty(), "lex: {ld:?}");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse: {pd:?}");
    let graph = ModuleGraph {
        modules: vec![Module {
            path: "test.ys".to_string(),
            ast,
            imports: Vec::new(),
        }],
    };
    let (res, cd) = check(&graph);
    assert!(cd.is_empty(), "check: {cd:?}");
    interpret(&graph, &res)
}

/// Full pipeline over in-memory files; the first file is the entry.
fn run_multi(files: &[(&str, &str)]) -> Result<Value, Diagnostic> {
    let store: HashMap<String, String> = files
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut read = |p: &str| {
        store
            .get(p)
            .cloned()
            .ok_or_else(|| "no such file".to_string())
    };
    let mut map = SourceMap::new();
    let (graph, fd) = load_program(files[0].0, &mut read, &mut map).unwrap();
    assert!(fd.is_empty(), "front-end: {fd:?}");
    let (res, cd) = check(&graph);
    assert!(cd.is_empty(), "check: {cd:?}");
    interpret(&graph, &res).map(|(value, _)| value)
}

#[test]
fn cross_module_calls_execute() {
    assert_eq!(
        run_multi(&[
            (
                "main.ys",
                "import { double } from \"./lib\";\nfun main(): int { return double(21); }"
            ),
            ("lib.ys", "export fun double(n: int): int { return n * 2; }"),
        ]),
        Ok(Value::Int(42))
    );
}

#[test]
fn same_named_functions_resolve_within_their_own_module() {
    // Both modules define `helper`; each function must call its own.
    assert_eq!(
        run_multi(&[
            (
                "main.ys",
                "import { a } from \"./a\";\n\
                     fun helper(): int { return 2; }\n\
                     fun main(): int { return a() + helper(); }"
            ),
            (
                "a.ys",
                "fun helper(): int { return 1; }\n\
                     export fun a(): int { return helper(); }"
            ),
        ]),
        Ok(Value::Int(3))
    );
}

#[test]
fn arithmetic_respects_precedence() {
    assert_eq!(
        run("fun main(): int { return 1 + 2 * 3; }"),
        Ok(Value::Int(7))
    );
}

#[test]
fn local_bindings_and_unary() {
    assert_eq!(
        run("fun main(): int { const x: int = 10; return -x + 2; }"),
        Ok(Value::Int(-8))
    );
}

#[test]
fn float_arithmetic() {
    assert_eq!(
        run("fun main(): float { return 1.5 * 2.0; }"),
        Ok(Value::Float(3.0))
    );
}

#[test]
fn no_main_returns_unit() {
    assert_eq!(run("fun other(): int { return 1; }"), Ok(Value::Unit));
}

#[test]
fn for_loops_iterate_and_return_early() {
    let program = "\
fun find(xs: int[], needle: int): bool {
    for x in xs { if x == needle { return true; } }
    return false;
}
fun main(): int {
    var xs: int[] = [3, 7, 42];
    var total: int = 0;
    for x in xs { total = total + x; }
    if find(xs, 7) && !find(xs, 9) { return total; }
    return 0;
}";
    assert_eq!(run(program), Ok(Value::Int(52)));
}

#[test]
fn widened_iterables_run_with_optional_elements() {
    let program = "\
fun main(): int {
    var acc: int = 0;
    for x in [10, null, 32] { acc = acc + (x ?? 0); }
    return acc;
}";
    assert_eq!(run(program), Ok(Value::Int(42)));
}

#[test]
fn for_loops_track_the_index_on_request() {
    let program = "\
fun main(): int {
    var xs: int[] = [10, 20, 30];
    var acc: int = 0;
    for [i, x] in xs { acc = acc + i * x; }
    return acc;
}";
    // 0*10 + 1*20 + 2*30
    assert_eq!(run(program), Ok(Value::Int(80)));
}

#[test]
fn arrays_roundtrip_with_builtins() {
    let program = "\
fun main(): int {
    var xs: int[] = [];
    push(xs, 10);
    push(xs, 20);
    push(xs, 12);
    xs[2] = xs[2] + 0;
    var i: int = 0;
    var sum: int = 0;
    while i < len(xs) { sum = sum + xs[i]; i = i + 1; }
    return sum;
}";
    assert_eq!(run(program), Ok(Value::Int(42)));
}

#[test]
fn arrays_alias_and_compare_by_identity() {
    let program = "\
fun main(): bool {
    const a: int[] = [1, 2];
    const b: int[] = a;
    b[0] = 9;
    return a[0] == 9 && a == b && a != [1, 2];
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn out_of_bounds_indexing_is_a_runtime_error() {
    let result = run("fun main(): int { const xs: int[] = [1]; return xs[5]; }");
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.message.contains("out of bounds")),
        "{result:?}"
    );
}

#[test]
fn value_structs_in_arrays_write_back_through_the_index() {
    let program = "\
struct P { x: int }
fun main(): int {
    var ps: P[] = [P { x: 1 }];
    ps[0].x = 7;
    return ps[0].x;
}";
    assert_eq!(run(program), Ok(Value::Int(7)));
}

#[test]
fn print_runs_and_returns_unit() {
    assert_eq!(
        run("fun main() { print(\"hi\"); print(1 + 1); print(null == null); }"),
        Ok(Value::Unit)
    );
}

#[test]
fn user_print_shadows_the_builtin_at_runtime() {
    assert_eq!(
        run("fun print(n: int): int { return n * 2; }\n\
                 fun main(): int { return print(21); }"),
        Ok(Value::Int(42))
    );
}

#[test]
fn division_by_zero_is_a_runtime_error() {
    assert!(run("fun main(): int { return 10 / 0; }").is_err());
}

#[test]
fn invalid_float_to_int_conversion_is_a_runtime_error() {
    // ADR 0028: NaN and out-of-range floats have no int value.
    assert!(run("fun main(): int { return int(0.0 / 0.0); }").is_err());
    assert!(run("fun main(): int { return int(1.0 / 0.0); }").is_err());
    assert!(run("fun main(): int { return int(9223372036854775808.0); }").is_err());
}

#[test]
fn string_conversion_renders_prints_text() {
    // ADR 0029: string(x) is exactly the text print(x) writes.
    assert_eq!(
        run("fun main(): string { return string(42) + string(true) + string(2.5); }"),
        Ok(Value::Str(b"42true2.5".to_vec()))
    );
    assert_eq!(
        run("struct P { b: bool, a: int }\n\
             fun main(): string { return string(P { b: true, a: 7 }); }"),
        Ok(Value::Str(b"P { a: 7, b: true }".to_vec()))
    );
    assert_eq!(
        run("fun main(): string { var o: int? = null; return string(o) + string([1, 2]); }"),
        Ok(Value::Str(b"null[1, 2]".to_vec()))
    );
}

#[test]
fn template_literals_render_like_print() {
    // ADR 0030: the desugar is string() + concat, so template text is
    // print's text — including optionals and the string pass-through.
    assert_eq!(
        run("fun main(): string { const p: int? = null; return `p=${p} q=${2.5} s=${\"x\"}`; }"),
        Ok(Value::Str(b"p=null q=2.5 s=x".to_vec()))
    );
}

#[test]
fn division_overflow_is_a_runtime_error() {
    // i64::MIN / -1 has no i64 result; like division by zero it must
    // be a diagnostic, not a panic (or SIGFPE once compiled).
    let min = "(0 - 9223372036854775807 - 1)";
    assert!(run(&format!("fun main(): int {{ return {min} / (0 - 1); }}")).is_err());
    assert!(run(&format!("fun main(): int {{ return {min} % (0 - 1); }}")).is_err());
}

#[test]
fn end_to_end_function_calls() {
    let program = "\
fun square(n: int): int { return n * n; }
fun main(): int {
    const a: int = square(3);
    var b: int = 4;
    b = b + a;
    return b;
}";
    assert_eq!(run(program), Ok(Value::Int(13)));
}

#[test]
fn nested_calls() {
    let program = "\
fun inc(n: int): int { return n + 1; }
fun main(): int { return inc(inc(inc(0))); }";
    assert_eq!(run(program), Ok(Value::Int(3)));
}

#[test]
fn comparisons_and_equality_evaluate() {
    assert_eq!(
        run("fun main(): bool { return 1 + 2 == 3; }"),
        Ok(Value::Bool(true))
    );
    assert_eq!(
        run("fun main(): bool { return 2.0 <= 1.5; }"),
        Ok(Value::Bool(false))
    );
    assert_eq!(
        run("fun main(): bool { return \"a\" != \"b\"; }"),
        Ok(Value::Bool(true))
    );
}

#[test]
fn logical_operators_short_circuit() {
    // The right side would divide by zero — short-circuiting must skip it.
    assert_eq!(
        run("fun main(): bool { return false && 1 / 0 == 0; }"),
        Ok(Value::Bool(false))
    );
    assert_eq!(
        run("fun main(): bool { return true || 1 / 0 == 0; }"),
        Ok(Value::Bool(true))
    );
    // And without short-circuit conditions, both sides evaluate.
    assert_eq!(
        run("fun main(): bool { return true && !false; }"),
        Ok(Value::Bool(true))
    );
}

#[test]
fn string_concatenation() {
    assert_eq!(
        run("fun main(): string { return \"foo\" + \"bar\"; }"),
        Ok(Value::Str(b"foobar".to_vec()))
    );
}

#[test]
fn if_else_selects_the_right_branch() {
    let abs = "\
fun abs(n: int): int {
    if n < 0 { return -n; } else { return n; }
}
fun main(): int { return abs(-7) + abs(7); }";
    assert_eq!(run(abs), Ok(Value::Int(14)));
}

#[test]
fn break_exits_the_loop_early() {
    let program = "\
fun main(): int {
    var i: int = 0;
    var acc: int = 0;
    while i < 100 {
        if i == 5 { break; }
        acc = acc + i;
        i = i + 1;
    }
    return acc * 1000 + i;
}";
    // 0+1+2+3+4 = 10; i stopped at 5.
    assert_eq!(run(program), Ok(Value::Int(10005)));
}

#[test]
fn continue_skips_but_still_advances() {
    let program = "\
fun main(): int {
    var acc: int = 0;
    var i: int = 0;
    while i < 10 {
        i = i + 1;
        if i % 2 == 0 { continue; }
        acc = acc + i;
    }
    const xs: int[] = [1, 2, 3, 4, 5];
    var evens: int = 0;
    for x in xs {
        if x % 2 == 1 { continue; }
        evens = evens + x;
    }
    return acc * 100 + evens;
}";
    // odds 1..10 sum to 25; evens in xs sum to 6. The `for` proves
    // continue advances to the next element instead of re-running one.
    assert_eq!(run(program), Ok(Value::Int(2506)));
}

#[test]
fn break_and_continue_bind_to_the_innermost_loop() {
    let program = "\
fun main(): int {
    var hits: int = 0;
    var i: int = 0;
    while i < 3 {
        i = i + 1;
        var j: int = 0;
        while true {
            j = j + 1;
            if j >= 2 { break; }
            if j == 1 { continue; }
            hits = hits + 100;
        }
        hits = hits + j;
    }
    return hits;
}";
    // Inner loop always exits at j == 2; outer runs all 3 iterations;
    // the += 100 line is continue-skipped every time.
    assert_eq!(run(program), Ok(Value::Int(6)));
}

#[test]
fn break_in_a_live_for_stops_the_growth() {
    let program = "\
fun main(): int {
    var xs: int[] = [1, 2];
    var seen: int = 0;
    for x in xs {
        seen = seen + 1;
        push(xs, x);
        if seen == 4 { break; }
    }
    return seen * 100 + len(xs);
}";
    // Live iteration would run forever (the body keeps pushing);
    // break must be what stops it, after exactly 4 elements.
    assert_eq!(run(program), Ok(Value::Int(406)));
}

#[test]
fn while_loop_accumulates() {
    let program = "\
fun main(): int {
    var i: int = 0;
    var acc: int = 0;
    while i < 5 {
        i = i + 1;
        acc = acc + i;
    }
    return acc;
}";
    assert_eq!(run(program), Ok(Value::Int(15)));
}

#[test]
fn block_scopes_shadow_and_expire() {
    // The inner `const x` shadows the outer `var x` inside the block only.
    let program = "\
fun main(): int {
    var x: int = 1;
    if true { const x: int = 10; }
    return x;
}";
    assert_eq!(run(program), Ok(Value::Int(1)));
}

#[test]
fn struct_literal_and_field_access() {
    let program = "\
struct Point { x: int, y: int }
fun main(): int {
    const p: Point = Point { x: 3, y: 4 };
    return p.x * p.x + p.y * p.y;
}";
    assert_eq!(run(program), Ok(Value::Int(25)));
}

#[test]
fn structs_pass_through_calls() {
    let program = "\
struct Point { x: int, y: int }
fun make(x: int, y: int): Point { return Point { x: x, y: y }; }
fun sum(p: Point): int { return p.x + p.y; }
fun main(): int { return sum(make(1, 2)); }";
    assert_eq!(run(program), Ok(Value::Int(3)));
}

#[test]
fn nested_struct_field_access() {
    let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): int {
    const o: Outer = Outer { i: Inner { v: 7 } };
    return o.i.v;
}";
    assert_eq!(run(program), Ok(Value::Int(7)));
}

#[test]
fn imported_struct_constructs_and_reads_across_modules() {
    assert_eq!(
        run_multi(&[
            (
                "main.ys",
                "import { Pair, make } from \"./lib\";\n\
                     fun main(): int { const p: Pair = make(); return p.a + Pair { a: 1, b: 2 }.b; }"
            ),
            (
                "lib.ys",
                "export struct Pair { a: int, b: int }\n\
                     export fun make(): Pair { return Pair { a: 40, b: 0 }; }"
            ),
        ]),
        Ok(Value::Int(42))
    );
}

#[test]
fn struct_equality_compares_fields() {
    let program = "\
struct Point { x: int, y: int }
fun main(): bool {
    const a: Point = Point { x: 1, y: 2 };
    const b: Point = Point { x: 1, y: 2 };
    const c: Point = Point { x: 9, y: 2 };
    return a == b && a != c;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn struct_equality_ignores_literal_field_order() {
    let program = "\
struct Point { x: int, y: int }
fun main(): bool {
    return Point { x: 1, y: 2 } == Point { y: 2, x: 1 };
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn nested_struct_equality_recurses() {
    let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): bool {
    const a: Outer = Outer { i: Inner { v: 1 } };
    const b: Outer = Outer { i: Inner { v: 2 } };
    return a != b;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn field_assignment_mutates_the_struct() {
    let program = "\
struct Point { x: int, y: int }
fun main(): int {
    var p: Point = Point { x: 1, y: 2 };
    p.x = 40;
    return p.x + p.y;
}";
    assert_eq!(run(program), Ok(Value::Int(42)));
}

#[test]
fn nested_field_assignment() {
    let program = "\
struct Inner { v: int }
struct Outer { i: Inner }
fun main(): int {
    var o: Outer = Outer { i: Inner { v: 1 } };
    o.i.v = 9;
    return o.i.v;
}";
    assert_eq!(run(program), Ok(Value::Int(9)));
}

#[test]
fn refstruct_aliases_share_mutation() {
    let program = "\
refstruct P { x: int }
fun main(): int {
    const a: P = P { x: 1 };
    const b: P = a;
    b.x = 5;
    return a.x;
}";
    assert_eq!(run(program), Ok(Value::Int(5)));
}

#[test]
fn functions_mutate_refstruct_arguments() {
    let program = "\
refstruct P { x: int }
fun bump(p: P) { p.x = p.x + 1; }
fun main(): int {
    const p: P = P { x: 1 };
    bump(p);
    bump(p);
    return p.x;
}";
    assert_eq!(run(program), Ok(Value::Int(3)));
}

#[test]
fn refstruct_equality_is_identity() {
    let program = "\
refstruct P { x: int }
fun main(): bool {
    const a: P = P { x: 1 };
    const b: P = P { x: 1 };
    const c: P = a;
    return a == c && a != b;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn value_struct_copies_stay_independent() {
    // Pins ADR 0005: plain structs copy; mutating the copy leaves the
    // original untouched.
    let program = "\
struct V { x: int }
fun main(): int {
    var a: V = V { x: 1 };
    var b: V = a;
    b.x = 9;
    return a.x;
}";
    assert_eq!(run(program), Ok(Value::Int(1)));
}

#[test]
fn value_struct_compares_ref_fields_by_identity() {
    let program = "\
refstruct R { v: int }
struct Box { r: R }
fun main(): bool {
    const r1: R = R { v: 1 };
    const r2: R = R { v: 1 };
    const a: Box = Box { r: r1 };
    const b: Box = Box { r: r1 };
    const c: Box = Box { r: r2 };
    return a == b && a != c;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn mutation_through_a_mixed_value_ref_chain() {
    let program = "\
refstruct R { v: int }
struct Box { r: R }
fun main(): int {
    const b: Box = Box { r: R { v: 1 } };
    b.r.v = 7;
    return b.r.v;
}";
    assert_eq!(run(program), Ok(Value::Int(7)));
}

#[test]
fn imported_refstruct_mutates_across_modules() {
    assert_eq!(
        run_multi(&[
            (
                "main.ys",
                "import { Counter, bump } from \"./lib\";\n\
                     fun main(): int { const c: Counter = Counter { n: 0 }; bump(c); bump(c); return c.n; }"
            ),
            (
                "lib.ys",
                "export refstruct Counter { n: int }\n\
                     export fun bump(c: Counter) { c.n = c.n + 1; }"
            ),
        ]),
        Ok(Value::Int(2))
    );
}

#[test]
fn optional_chaining_short_circuits_on_null() {
    let program = "\
refstruct P { x: int }
fun get(p: P?): int { return p?.x ?? 42; }
fun main(): int { return get(null) + get(P { x: 1 }); }";
    assert_eq!(run(program), Ok(Value::Int(43)));
}

#[test]
fn null_equality_at_runtime() {
    let program = "\
refstruct P { x: int }
fun main(): bool {
    var p: P? = null;
    const was_null: bool = p == null;
    p = P { x: 1 };
    return was_null && p != null;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn linked_list_builds_traverses_and_mutates() {
    let program = "\
refstruct Node { v: int, next: Node? }
fun main(): int {
    const head: Node = Node { v: 1, next: Node { v: 2, next: Node { v: 3, next: null } } };
    var cur: Node? = head;
    var sum: int = 0;
    while cur != null {
        cur.v = cur.v * 10;
        sum = sum + cur.v;
        cur = cur.next;
    }
    return sum;
}";
    assert_eq!(run(program), Ok(Value::Int(60)));
}

#[test]
fn cyclic_values_render_finitely() {
    let program = "\
refstruct Node { v: int, next: Node? }
fun main(): Node {
    const a: Node = Node { v: 1, next: null };
    a.next = a;
    return a;
}";
    let (value, heap) = run_full(program).unwrap();
    let rendered = value.render(&heap);
    assert!(
        rendered.contains("Node") && rendered.contains("..."),
        "{rendered}"
    );
    assert!(rendered.len() < 500, "unbounded: {} bytes", rendered.len());
}

#[test]
fn scalar_rendering_matches_debug() {
    let heap = Heap::default();
    assert_eq!(Value::Int(55).render(&heap), "Int(55)");
    assert_eq!(Value::Bool(true).render(&heap), "Bool(true)");
}

#[test]
fn display_shows_user_values_not_enum_internals() {
    let mut heap = Heap::default();
    heap.arrays
        .push(vec![Value::Int(1), Value::Str(b"x".to_vec()), Value::Null]);
    let array = Value::Array(0);
    assert_eq!(array.display(&heap), b"[1, x, null]");
    let s = Value::Struct {
        name: "P".into(),
        fields: vec![("x".into(), Value::Int(1))],
    };
    assert_eq!(s.display(&heap), b"P { x: 1 }");
    heap.structs.push(StructObj {
        name: "N".into(),
        fields: vec![("v".into(), Value::Bool(true))],
    });
    let r = Value::Ref(0);
    assert_eq!(r.display(&heap), b"N { v: true }");
}

/// The kitchen-sink program: a binary search tree combining refstruct
/// mutation through const/param bindings, optional args and returns,
/// else-branch narrowing, recursion, `??` laziness, and ref identity.
#[test]
fn binary_search_tree_end_to_end() {
    let program = "\
refstruct Tree { v: int, left: Tree?, right: Tree? }
fun insert(t: Tree?, v: int): Tree {
    if t == null {
        return Tree { v: v, left: null, right: null };
    } else {
        if v < t.v { t.left = insert(t.left, v); } else { t.right = insert(t.right, v); }
        return t;
    }
}
fun sum(t: Tree?): int {
    if t == null { return 0; } else { return sum(t.left) + t.v + sum(t.right); }
}
fun min(t: Tree): int {
    var cur: Tree = t;
    while cur.left != null { cur = cur.left; }
    return cur.v;
}
fun main(): bool {
    var root: Tree? = null;
    root = insert(root, 5);
    const keep: Tree? = root;
    root = insert(root, 3);
    root = insert(root, 8);
    root = insert(root, 1);
    return sum(root) == 17 && keep == root && min(keep ?? insert(null, 0)) == 1;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn field_narrowed_traversal_runs() {
    let program = "\
refstruct Node { v: int, next: Node? }
fun last(head: Node): int {
    var cur: Node = head;
    while cur.next != null { cur = cur.next; }
    return cur.v;
}
fun main(): int {
    return last(Node { v: 1, next: Node { v: 2, next: Node { v: 3, next: null } } });
}";
    assert_eq!(run(program), Ok(Value::Int(3)));
}

#[test]
fn coalesce_chains_left_associatively() {
    let program = "\
fun main(): int {
    var a: int? = null;
    var b: int? = null;
    return (a ?? b ?? 7) + (a ?? 1);
}";
    assert_eq!(run(program), Ok(Value::Int(8)));
}

#[test]
fn returned_refstruct_keeps_identity() {
    let program = "\
refstruct P { x: int }
fun same(p: P): P { return p; }
fun main(): bool {
    const a: P = P { x: 1 };
    return same(a) == a;
}";
    assert_eq!(run(program), Ok(Value::Bool(true)));
}

#[test]
fn deep_expressions_with_recursion_are_a_diagnostic_not_a_crash() {
    // Expression nesting recurses natively too — it must draw from the
    // same depth budget as calls instead of overflowing the stack.
    let program = "\
fun down(n: int): int {
    if n == 0 { return 0; }
    return 0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + (0 + down(n - 1))))))))));
}
fun main(): int { return down(100000); }";
    let result = run(program);
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.message.contains("depth limit")),
        "{result:?}"
    );
}

#[test]
fn runaway_allocation_is_a_diagnostic_not_an_oom() {
    // Loop temporaries land in the arena; a cap turns runaway
    // allocation into a sanctioned diagnostic instead of an OOM kill.
    let program = "\
fun main() {
    var i: int = 0;
    while i < 400000 {
        const xs: int[][] = [[1], [2], [3], [4]];
        i = i + 1;
    }
}";
    let result = run(program);
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.message.contains("heap limit")),
        "{result:?}"
    );
}

#[test]
fn ref_hops_consume_render_depth() {
    // A two-ref-field cycle must stay bounded like the Rc oracle did
    // (hop + struct each cost a level), not fan out exponentially.
    let program = "\
refstruct T { a: T?, b: T? }
fun main(): T {
    const t: T = T { a: null, b: null };
    t.a = t;
    t.b = t;
    return t;
}";
    let (value, heap) = run_full(program).unwrap();
    let rendered = value.render(&heap);
    assert!(rendered.len() < 600, "fan-out: {} bytes", rendered.len());
}

#[test]
fn runaway_recursion_is_a_diagnostic_not_a_crash() {
    let result = run("fun f(): int { return f(); }\nfun main(): int { return f(); }");
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.message.contains("depth limit")),
        "{result:?}"
    );
}

#[test]
fn deep_but_bounded_recursion_still_runs() {
    let program = "\
fun down(n: int): int { if n == 0 { return 0; } return down(n - 1); }
fun main(): int { return down(4000); }";
    assert_eq!(run(program), Ok(Value::Int(0)));
}

#[test]
fn recursion_with_control_flow() {
    let fib = "\
fun fib(n: int): int {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fun main(): int { return fib(10); }";
    assert_eq!(run(fib), Ok(Value::Int(55)));
}
