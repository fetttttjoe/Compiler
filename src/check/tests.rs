use super::*;
use crate::modules::{Module, load_program};
use crate::source::SourceMap;
use crate::{lexer::lex, parser::parse};

fn graph_of(src: &str) -> ModuleGraph {
    let (tokens, ld) = lex(src);
    assert!(ld.is_empty(), "lex: {ld:?}");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse: {pd:?}");
    ModuleGraph {
        modules: vec![Module {
            path: "test.ys".to_string(),
            ast,
            imports: Vec::new(),
        }],
    }
}

/// Checks a multi-file program (first file = entry) through the real
/// module loader.
fn multi(files: &[(&str, &str)]) -> (Resolutions, Vec<Diagnostic>) {
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
    check(&graph)
}

#[test]
fn resolves_local_names_to_the_defining_module() {
    let (res, d) = check(&graph_of(
        "struct P { x: int } fun f(a: int): float { return 1.0; }",
    ));
    assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
    assert_eq!(res.functions[0]["f"], (0, "f".to_string()));
}

#[test]
fn duplicate_function_is_an_error() {
    let d = diags("fun f(): int { return 1; } fun f(): int { return 2; }");
    assert_eq!(d.len(), 1);
    assert!(d[0].message.contains("already defined"));
}

#[test]
fn unknown_type_is_an_error() {
    let (_, d) = check(&graph_of("fun f(a: Missing): int { return 1; }"));
    assert!(
        d.iter()
            .any(|e| e.message.contains("unknown type 'Missing'"))
    );
}

fn diags(src: &str) -> Vec<Diagnostic> {
    check(&graph_of(src)).1
}

#[test]
fn conversions_are_strictly_cross_typed() {
    // ADR 0028: float() takes an int, int() takes a float; identity
    // conversions are rejected with a help note.
    let d = diags("fun f() { const x: float = float(1.5); }");
    assert!(d[0].message.contains("float() expects int, found float"));
    let d = diags("fun f() { const x: int = int(1); }");
    assert!(d[0].message.contains("int() expects float, found int"));
    let d = diags("fun f() { const x: int = int(true); }");
    assert!(d[0].message.contains("int() expects float, found bool"));
    assert!(diags("fun f(): int { return int(float(41) + 1.0); }").is_empty());
}

#[test]
fn string_conversion_accepts_values_and_rejects_no_ops() {
    // ADR 0029: every value type converts; the identity and the
    // no-value types (unit, null) are rejected.
    assert!(diags("fun f(): string { return string(1) + string(0.5) + string(true); }").is_empty());
    assert!(diags("struct P { x: int }\nfun f(p: P): string { return string(p); }").is_empty());
    assert!(diags("fun f(o: int?): string { return string(o); }").is_empty());
    let d = diags("fun f(s: string): string { return string(s); }");
    assert!(d[0].message.contains("string() cannot convert string"));
    assert_eq!(d[0].help.as_deref(), Some("the value is already string"));
    let d = diags("fun g() {}\nfun f() { const s: string = string(g()); }");
    assert!(d[0].message.contains("string() cannot convert unit"));
    let d = diags("fun f() { const s: string = string(null); }");
    assert!(d[0].message.contains("string() cannot convert null"));
    // Narrowing applies: inside the guard the payload IS a string.
    let d = diags("fun f(o: string?): string { if o != null { return string(o); } return \"\"; }");
    assert!(d[0].message.contains("string() cannot convert string"));
}

#[test]
fn world_interface_builtins_are_typed() {
    // ADR 0031: open/read/readLine/write/close signatures, both
    // readLine arities, and the widened entry rule.
    assert!(
        diags(
            "fun main(args: string[]): int {
            const f: file? = open(\"x\", \"r\");
            if f != null {
                const s: string? = readLine(f);
                const c: string? = read(f, 4);
                const ok: bool = write(f, \"data\");
                const done: bool = close(f);
            }
            const l: string? = readLine();
            return len(args);
        }"
        )
        .is_empty()
    );
    let d = diags("fun f() { const x: file? = open(1, \"r\"); }");
    assert!(d[0].message.contains("'open' expects string, found int"));
    let d = diags("fun f(g: file) { read(g, \"x\"); }");
    assert!(d[0].message.contains("'read' expects int, found string"));
    let d = diags("fun f(g: file) { close(g, g); }");
    assert!(d[0].message.contains("'close' expects 1 argument, found 2"));
    let d = diags("fun f(g: file?) { close(g); }");
    assert!(d[0].message.contains("'close' expects file, found file?"));
    let d = diags("fun main(a: int): int { return a; }");
    assert!(
        d[0].message
            .contains("takes no parameters or exactly (args: string[])")
    );
    assert!(diags("fun main(args: string[]): int { return len(args); }").is_empty());
}

#[test]
fn templates_interpolate_strings_but_not_unit_or_null() {
    // ADR 0030: `${s}` passes a string through (the implicit form);
    // unit and null still have no text.
    assert!(diags("fun f(name: string, n: int): string { return `${name}:${n}`; }").is_empty());
    let d = diags("fun g() {}\nfun f(): string { return `${g()}`; }");
    assert!(
        d[0].message
            .contains("cannot interpolate unit in a template")
    );
    let d = diags("fun f(): string { return `${null}`; }");
    assert!(
        d[0].message
            .contains("cannot interpolate null in a template")
    );
}

#[test]
fn diverging_guards_narrow_the_code_after_the_if() {
    // ADR 0020: a branch that never falls through leaves the negation
    // facts (or the fall-through branch's survivors) behind.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
         fun top(p: Node?): int {\n\
             if p == null { return 0; }\n\
             return p.v;\n\
         }\n\
         fun skip(head: Node?): int {\n\
             var cur: Node? = head;\n\
             var acc: int = 0;\n\
             while cur != null {\n\
                 if cur.v == 2 { cur = cur.next; continue; }\n\
                 acc = acc + cur.v;\n\
                 cur = cur.next;\n\
             }\n\
             return acc;\n\
         }\n\
         fun find(head: Node?): int {\n\
             var cur: Node? = head;\n\
             while true {\n\
                 if cur == null { break; }\n\
                 if cur.v == 9 { return cur.v; }\n\
                 cur = cur.next;\n\
             }\n\
             return 0;\n\
         }\n\
         fun via_else(p: Node?): int {\n\
             if p != null { print(p.v); } else { return 0; }\n\
             return p.v;\n\
         }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn divergence_narrowing_keeps_the_unsound_cases_rejected() {
    // Fall-through branch reassigns: its survivors are empty.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
         fun f(p: Node?): int {\n\
             var q: Node? = p;\n\
             if q == null { return 0; } else { q = null; }\n\
             return q.v;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");

    // A non-diverging guard proves nothing after the if.
    let d = diags(
        "refstruct Node { v: int }\n\
         fun f(p: Node?): int {\n\
             if p == null { print(0); }\n\
             return p.v;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");

    // A break inside a nested loop does not make the branch diverge.
    let d = diags(
        "refstruct Node { v: int }\n\
         fun f(p: Node?): int {\n\
             if p == null { while true { break; } }\n\
             return p.v;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");

    // Guard facts die with their block.
    let d = diags(
        "refstruct Node { v: int }\n\
         fun f(p: Node?, b: bool): int {\n\
             if b { if p == null { return 0; } print(p.v); }\n\
             return p.v;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");

    // A call after a field-path guard still kills the field fact.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
         fun g() {}\n\
         fun f(n: Node): int {\n\
             if n.next == null { return 0; }\n\
             g();\n\
             return n.next.v;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn break_and_continue_outside_a_loop_are_errors() {
    let d = diags("fun f() { break; }");
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].message.contains("'break' outside of a loop"), "{d:?}");

    // Inside an `if` is still outside a loop.
    let d = diags("fun f(b: bool) { if b { continue; } }");
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(
        d[0].message.contains("'continue' outside of a loop"),
        "{d:?}"
    );
}

#[test]
fn break_and_continue_are_accepted_inside_loops() {
    let d = diags(
        "fun f(xs: int[]): int {\n\
             var n: int = 0;\n\
             while n < 10 { if n == 5 { break; } n = n + 1; }\n\
             for x in xs { if x == 0 { continue; } n = n + x; }\n\
             return n;\n\
         }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");

    // The loop context ends with the loop: a `break` after one is an error.
    let d = diags("fun f() { while false { } break; }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn arithmetic_of_same_type_is_ok() {
    let d = diags("fun f(a: int, b: int): int { return a + b * 2; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn mixing_int_and_float_is_an_error() {
    let d = diags("fun f(a: int): int { const x: float = a + 1.0; return a; }");
    assert!(
        d.iter().any(|e| e.message.contains("cannot apply '+'")),
        "{d:?}"
    );
}

#[test]
fn undefined_variable_is_an_error() {
    let d = diags("fun f(): int { return missing; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("undefined variable 'missing'")),
        "{d:?}"
    );
}

// --- Recovery must not cascade: one mistake, one error ---

#[test]
fn recovery_does_not_cascade_into_return_checks() {
    let d = diags("fun f(): int { return missing; }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn recovery_does_not_cascade_through_operators() {
    let d = diags("fun f(): int { return missing + 1 * 2; }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn null_deref_reports_exactly_one_error() {
    let d = diags("refstruct P { x: int } fun f(p: P?): int { return p.x; }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn unknown_callee_still_checks_arguments_without_cascading() {
    // The bad callee is one error; the bad argument is a second, real
    // one — but no third from the return-type check.
    let d = diags("fun f(): int { return nope(missing); }");
    assert_eq!(d.len(), 2, "{d:?}");
}

// --- Empty literals, poison structure, cascades (review findings) ---

#[test]
fn unannotated_empty_array_requires_an_annotation() {
    let d = diags("fun main() { var xs = []; push(xs, 1); }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("missing type annotation") && e.help.is_some()),
        "{d:?}"
    );
}

#[test]
fn empty_literal_fits_optional_array_slots() {
    let d = diags("fun f(xs: int[]?) { } fun main() { var x: int[]? = []; f([]); }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn empty_literals_nest_in_either_position() {
    let d = diags("fun main() { const a: int[][] = [[1], []]; const b: int[][] = [[], [1]]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn null_elements_widen_arrays_to_optional() {
    let d = diags("fun main() { const xs: int?[] = [1, null, 2]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn unknown_types_poison_structurally_without_cascades() {
    let d = diags("fun main() { var xs: Missing[] = [1, 2]; }");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = diags("fun f(x: Foo?) { } fun main() { f(1); }");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = diags("fun f(): Missing { }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn operator_errors_do_not_cascade() {
    let d = diags("fun f(): int { return -\"a\"; }");
    assert_eq!(d.len(), 1, "{d:?}");
    let d = diags("fun f(xs: int[]?): int[] { return xs ?? 1; }");
    assert_eq!(d.len(), 1, "{d:?}");
}

#[test]
fn empty_literal_into_a_non_array_names_it_cleanly() {
    let d = diags("fun f(a: int) { } fun main() { f([]); }");
    assert!(d.iter().any(|e| e.message.contains("found []")), "{d:?}");
}

// --- Literal-vs-declaration checking at every declared position ---

#[test]
fn literal_checking_recurses_into_nested_literals() {
    let d = diags("fun f() { var g: int?[][] = [[1, 2], [3]]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f() { var g: int?[][] = [[null]]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn literal_checking_unwraps_optional_declarations() {
    let d = diags("fun f() { var xs: int?[]? = [1, 2]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn literal_checking_applies_at_every_declared_position() {
    let d = diags(
        "struct Box { xs: int?[] }\n\
             fun g(xs: int?[]): int?[] { return [1, 2]; }\n\
             fun f() {\n\
                 var xs: int?[] = [1, 2];\n\
                 xs = [3, 4];\n\
                 g([5, 6]);\n\
                 const b: Box = Box { xs: [7, 8] };\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn null_elements_against_non_optional_declarations_get_a_hint() {
    let d = diags("fun f() { var xs: int[] = [null, 1]; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected int, found null")
                && e.help.as_deref().is_some_and(|h| h.contains("optional"))),
        "{d:?}"
    );
}

#[test]
fn missing_annotation_error_leads_for_uninferable_literals() {
    let d = diags("fun f() { var xs = [null]; }");
    assert!(d[0].message.contains("missing type annotation"), "{d:?}");
}

#[test]
fn iterating_an_unconstrained_literal_is_an_error() {
    // `for x in [[]]` must not bind x at a type that fits everything.
    let d = diags("fun f() { for x in [[]] { print(x); } }");
    assert!(
        d.iter().any(|e| e.message.contains("cannot infer")),
        "{d:?}"
    );
}

#[test]
fn iterable_position_still_infers_literal_types() {
    // Inference survives where nothing is declared: loop iterables.
    let d = diags("fun f() { for x in [1, null] { print(x ?? 0); } }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f() { for x in [[], [1]] { print(len(x)); } }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f() { for x in [1, \"a\"] { } }");
    assert!(
        d.iter().any(|e| e.message.contains("must share one type")),
        "{d:?}"
    );
}

// --- Mandatory annotations (ADR 0010) ---

#[test]
fn every_binding_requires_a_type_annotation() {
    let d = diags("fun f() { var x = 5; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("missing type annotation for 'x'") && e.help.is_some()),
        "{d:?}"
    );
}

#[test]
fn array_literals_are_checked_against_the_declaration() {
    // The annotation is the source of truth: int elements are welcome
    // in an int?[] — no inference from the literal's shape.
    let d = diags("fun f() { var xs: int?[] = [1, 2]; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f() { var xs: int?[] = [1, \"a\"]; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected int?, found string")),
        "{d:?}"
    );
}

// --- For loops ---

#[test]
fn for_loops_type_the_element_and_bind_it_const() {
    let d = diags(
        "fun sum(xs: int[]): int { var total: int = 0; for x in xs { total = total + x; } return total; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f(xs: int[]) { for x in xs { x = 1; } }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'x'")),
        "{d:?}"
    );
    let d = diags("fun f(n: int) { for x in n { } }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("can only iterate over arrays")),
        "{d:?}"
    );
}

#[test]
fn for_bodies_invalidate_enclosing_facts_like_while() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(q: P?, xs: int[]): int {\n\
                 var p: P? = q;\n\
                 var total: int = 0;\n\
                 if p != null {\n\
                     for x in xs { total = total + p.x; p = null; }\n\
                 }\n\
                 return total;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn for_index_bindings_are_int_and_const() {
    let d = diags("fun f(xs: string[]) { for [i, s] in xs { print(s); print(i + 1); } }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f(xs: int[]) { for [i, x] in xs { i = 0; } }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'i'")),
        "{d:?}"
    );
    let d = diags("fun f(xs: int[]) { for [x, x] in xs { } }");
    assert!(
        d.iter().any(|e| e.message.contains("distinct names")),
        "{d:?}"
    );
}

// --- Builtins ---

#[test]
fn print_accepts_any_single_value() {
    let d = diags(
        "struct P { x: int }\n\
             fun main() { print(1); print(\"x\"); print(true); print(P { x: 1 }); }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn print_arity_is_checked() {
    let d = diags("fun main() { print(); }");
    assert!(
        d.iter().any(|e| e.message.contains("expects 1 argument")),
        "{d:?}"
    );
}

// --- Arrays ---

#[test]
fn array_literals_indexing_and_builtins_are_typed() {
    let d = diags(
        "fun f(): int { const xs: int[] = [1, 2, 3]; return xs[0] + len(xs); }\n\
             fun g() { var ys: int[] = []; push(ys, 4); }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn array_elements_must_share_one_type() {
    let d = diags("fun f() { const xs: int[] = [1, \"a\"]; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected int, found string")),
        "{d:?}"
    );
}

#[test]
fn index_must_be_int_and_base_must_be_array() {
    let d = diags("fun f(xs: int[]): int { return xs[\"a\"]; }");
    assert!(
        d.iter().any(|e| e.message.contains("index must be int")),
        "{d:?}"
    );
    let d = diags("fun f(): int { return 1[0]; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot index into int")),
        "{d:?}"
    );
}

#[test]
fn push_checks_the_element_type() {
    let d = diags("fun f(xs: int[]) { push(xs, \"a\"); }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected int, found string")),
        "{d:?}"
    );
}

#[test]
fn arrays_are_references_for_mutability() {
    // Writing an element goes through the array reference — fine on a
    // const binding. Rebinding the const is still an error.
    let d = diags("fun f() { const xs: int[] = [1]; xs[0] = 2; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f() { const xs: int[] = [1]; xs = [2]; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'xs'")),
        "{d:?}"
    );
}

#[test]
fn user_definitions_shadow_builtins() {
    let d = diags(
        "fun print(n: int): int { return n; }\n\
             fun main(): int { return print(3); }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn comparisons_equality_and_logic_are_well_typed() {
    let d = diags("fun f(a: int, b: int): bool { return a < b && a != b || !(a >= b); }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn logical_operators_require_bools() {
    let d = diags("fun f(a: int, b: int): bool { return a && b; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot apply '&&' to int and int")),
        "{d:?}"
    );
}

#[test]
fn equality_requires_matching_types() {
    let d = diags("fun f(): bool { return 1 == 1.0; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot apply '==' to int and float")),
        "{d:?}"
    );
}

#[test]
fn struct_equality_is_well_typed() {
    let d = diags(
        "struct P { x: int }\n\
             fun f(a: P, b: P): bool { return a == b || a != b; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn struct_ordering_is_rejected() {
    let d = diags(
        "struct P { x: int }\n\
             fun f(a: P, b: P): bool { return a < b; }",
    );
    assert!(
        d.iter().any(|e| e.message.contains("cannot apply '<'")),
        "{d:?}"
    );
}

#[test]
fn equality_across_distinct_struct_types_is_rejected() {
    // Same short name in two modules — identity is (module, name).
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { make } from \"./lib\";\n\
                 struct P { x: int }\n\
                 fun f(): bool { return P { x: 1 } == make(); }",
        ),
        (
            "lib.ys",
            "export struct P { x: int }\n\
                 export fun make(): P { return P { x: 1 }; }",
        ),
    ]);
    assert!(
        d.iter().any(|e| e.message.contains("cannot apply '=='")),
        "{d:?}"
    );
}

#[test]
fn not_requires_bool() {
    let d = diags("fun f(a: int): bool { return !a; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot apply '!' to int")),
        "{d:?}"
    );
}

#[test]
fn string_concat_is_well_typed_and_mixed_concat_is_not() {
    let d = diags("fun f(s: string): string { return s + \"!\"; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
    let d = diags("fun f(s: string): string { return s + 1; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot apply '+' to string and int")),
        "{d:?}"
    );
}

#[test]
fn if_and_while_conditions_must_be_bool() {
    let d = diags("fun f(a: int): int { if a { return 1; } return 0; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("if condition must be bool, found int")),
        "{d:?}"
    );
    let d = diags("fun f(a: int): int { while a { a = a - 1; } return a; }");
    assert!(
        d.iter().any(|e| e
            .message
            .contains("while condition must be bool, found int")),
        "{d:?}"
    );
}

#[test]
fn block_bindings_do_not_escape_their_scope() {
    let d = diags("fun f(a: bool): int { if a { const x: int = 1; } return x; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("undefined variable 'x'")),
        "{d:?}"
    );
}

#[test]
fn missing_return_paths_are_errors() {
    // Empty body.
    let d = diags("fun f(): int { }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("not all paths in function 'f' return")),
        "{d:?}"
    );
    // If without else can fall through.
    let d = diags("fun f(a: bool): int { if a { return 1; } }");
    assert!(
        d.iter().any(|e| e.message.contains("not all paths")),
        "{d:?}"
    );
    // While may run zero times.
    let d = diags("fun f(): int { while true { return 1; } }");
    assert!(
        d.iter().any(|e| e.message.contains("not all paths")),
        "{d:?}"
    );
}

#[test]
fn both_branches_returning_satisfies_definite_return() {
    let d = diags("fun f(a: bool): int { if a { return 1; } else { return 2; } }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn imported_functions_and_structs_are_usable() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { make, Point } from \"./geo\";\n\
                 fun main(): int { const p: Point = make(); const q: Point = Point { x: p.x }; return q.x; }",
        ),
        (
            "geo.ys",
            "export struct Point { x: int }\n\
                 export fun make(): Point { return Point { x: 7 }; }",
        ),
    ]);
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn unknown_and_unexported_imports_have_distinct_errors() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { nope } from \"./lib\"; fun main(): int { return 1; }",
        ),
        ("lib.ys", "fun hidden(): int { return 1; }"),
    ]);
    assert!(
        d.iter().any(|e| e.message.contains("has no item 'nope'")),
        "{d:?}"
    );

    let (_, d) = multi(&[
        (
            "main.ys",
            "import { hidden } from \"./lib\"; fun main(): int { return 1; }",
        ),
        ("lib.ys", "fun hidden(): int { return 1; }"),
    ]);
    assert!(
        d.iter().any(|e| e
            .message
            .contains("'hidden' exists in 'lib.ys' but is not exported")),
        "{d:?}"
    );
}

#[test]
fn import_collisions_within_one_file_are_errors() {
    // Import colliding with a local definition.
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { f } from \"./lib\";\nfun f(): int { return 2; }\nfun main(): int { return f(); }",
        ),
        ("lib.ys", "export fun f(): int { return 1; }"),
    ]);
    assert!(
        d.iter()
            .any(|e| e.message.contains("'f' is already defined in this file")),
        "{d:?}"
    );

    // Import colliding with another import.
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { f } from \"./a\"; import { f } from \"./b\"; fun main(): int { return f(); }",
        ),
        ("a.ys", "export fun f(): int { return 1; }"),
        ("b.ys", "export fun f(): int { return 2; }"),
    ]);
    assert!(
        d.iter()
            .any(|e| e.message.contains("'f' is already defined in this file")),
        "{d:?}"
    );
}

#[test]
fn same_names_in_different_modules_coexist() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { a } from \"./a\"; import { b } from \"./b\";\n\
                 fun main(): int { return a() + b(); }",
        ),
        (
            "a.ys",
            "fun helper(): int { return 1; } export fun a(): int { return helper(); }",
        ),
        (
            "b.ys",
            "fun helper(): int { return 2; } export fun b(): int { return helper(); }",
        ),
    ]);
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn same_named_structs_in_different_modules_are_distinct_types() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { make } from \"./a\"; import { take } from \"./b\";\n\
                 fun main(): int { return take(make()); }",
        ),
        (
            "a.ys",
            "export struct P { x: int }\nexport fun make(): P { return P { x: 1 }; }",
        ),
        (
            "b.ys",
            "export struct P { x: int }\nexport fun take(p: P): int { return p.x; }",
        ),
    ]);
    // a.P and b.P share a name but are different types — and the message
    // says where each one lives.
    assert!(
        d.iter().any(|e| e
            .message
            .contains("expected argument of type P (from b.ys), found P (from a.ys)")),
        "{d:?}"
    );
}

#[test]
fn undefined_names_suggest_close_matches() {
    let d = diags("fun f(): int { const account: int = 1; return acount; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("undefined variable 'acount'")
                && e.help.as_deref() == Some("did you mean 'account'?")),
        "{d:?}"
    );

    let d = diags(
        "fun fibonacci(n: int): int { return n; }\n\
             fun main(): int { const answer: int = 1; return fibonaci(answr); }",
    );
    assert!(
        d.iter()
            .any(|e| e.message.contains("undefined function 'fibonaci'")
                && e.help.as_deref() == Some("did you mean 'fibonacci'?")),
        "{d:?}"
    );
    // Arguments are still checked even when the callee is unknown.
    assert!(
        d.iter()
            .any(|e| e.message.contains("undefined variable 'answr'")
                && e.help.as_deref() == Some("did you mean 'answer'?")),
        "{d:?}"
    );

    let d =
        diags("struct Point { x: int } fun f(): int { const p: Pont = Pont { x: 1 }; return 1; }");
    assert!(
        d.iter().any(|e| e.message.contains("unknown struct 'Pont'")
            && e.help.as_deref() == Some("did you mean 'Point'?")),
        "{d:?}"
    );
}

#[test]
fn unexported_import_gets_an_export_hint() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { hidden } from \"./lib\"; fun main(): int { return 1; }",
        ),
        ("lib.ys", "fun hidden(): int { return 1; }"),
    ]);
    assert!(
        d.iter().any(|e| e.message.contains("is not exported")
            && e.help
                .as_deref()
                .is_some_and(|h| h.contains("add 'export' before the definition of 'hidden'"))),
        "{d:?}"
    );
}

#[test]
fn misspelled_import_suggests_an_exported_name() {
    let (_, d) = multi(&[
        (
            "main.ys",
            "import { doubel } from \"./lib\"; fun main(): int { return 1; }",
        ),
        ("lib.ys", "export fun double(n: int): int { return n * 2; }"),
    ]);
    assert!(
        d.iter().any(|e| e.message.contains("has no item 'doubel'")
            && e.help.as_deref() == Some("did you mean 'double'?")),
        "{d:?}"
    );
}

#[test]
fn unit_functions_need_no_return() {
    let d = diags("fun f(a: int) { f(a - 1); }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn return_type_mismatch_is_an_error() {
    let d = diags("fun f(): int { return 1.0; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected return type int")),
        "{d:?}"
    );
}

#[test]
fn well_typed_program_passes() {
    let d = diags(
        "struct P { x: int, y: int }\n\
             fun add(a: int, b: int): int { return a + b; }\n\
             fun main(): int { const p: P = P { x: 1, y: 2 }; return add(p.x, p.y); }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn wrong_argument_count_is_an_error() {
    let d = diags("fun f(a: int): int { return a; } fun g(): int { return f(); }");
    assert!(
        d.iter().any(|e| e.message.contains("expects 1 argument")),
        "{d:?}"
    );
}

#[test]
fn argument_type_mismatch_is_an_error() {
    let d = diags("fun f(a: int): int { return a; } fun g(): int { return f(1.0); }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("expected argument of type int")),
        "{d:?}"
    );
}

#[test]
fn unknown_field_is_an_error() {
    let d = diags("struct P { x: int } fun f(p: P): int { return p.z; }");
    assert!(
        d.iter().any(|e| e.message.contains("no field 'z'")),
        "{d:?}"
    );
}

#[test]
fn struct_literal_field_type_mismatch_is_an_error() {
    let d = diags("struct P { x: int } fun f(): int { const p: P = P { x: 1.0 }; return 1; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("field 'x' expects int")),
        "{d:?}"
    );
}

#[test]
fn duplicate_struct_literal_field_is_an_error() {
    let d = diags("struct P { x: int } fun f(): int { const p: P = P { x: 1, x: 2 }; return 1; }");
    assert!(
        d.iter().any(|e| e.message.contains("duplicate field 'x'")),
        "{d:?}"
    );
}

#[test]
fn assign_type_mismatch_is_an_error() {
    let d = diags("fun f(): int { var x: int = 1; x = 1.0; return x; }");
    assert!(
        d.iter().any(|e| e
            .message
            .contains("cannot assign float to variable of type int")),
        "{d:?}"
    );
}

#[test]
fn missing_struct_literal_field_is_an_error() {
    let d =
        diags("struct P { x: int, y: int } fun f(): int { const p: P = P { x: 1 }; return 1; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("missing field 'y' in struct 'P'")),
        "{d:?}"
    );
}

#[test]
fn field_assignment_is_well_typed() {
    let d = diags("struct P { x: int } fun f() { var p: P = P { x: 1 }; p.x = 2; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn field_assignment_type_mismatch_is_an_error() {
    let d = diags("struct P { x: int } fun f() { var p: P = P { x: 1 }; p.x = 1.0; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("field 'x' expects int, found float")),
        "{d:?}"
    );
}

#[test]
fn field_assignment_through_const_is_an_error() {
    let d = diags("struct P { x: int } fun f() { const p: P = P { x: 1 }; p.x = 2; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'p'")),
        "{d:?}"
    );
}

#[test]
fn assigning_to_unknown_field_is_an_error() {
    let d = diags("struct P { x: int } fun f() { var p: P = P { x: 1 }; p.z = 2; }");
    assert!(
        d.iter().any(|e| e.message.contains("no field 'z'")),
        "{d:?}"
    );
}

#[test]
fn refstruct_field_mutation_through_const_is_allowed() {
    let d = diags("refstruct P { x: int } fun f() { const p: P = P { x: 1 }; p.x = 2; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn refstruct_param_mutation_is_allowed() {
    let d = diags("refstruct P { x: int } fun g(p: P) { p.x = 5; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn rebinding_const_refstruct_is_rejected() {
    let d = diags("refstruct P { x: int } fun f() { const p: P = P { x: 1 }; p = P { x: 2 }; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'p'")),
        "{d:?}"
    );
}

#[test]
fn value_struct_param_mutation_stays_rejected() {
    let d = diags("struct V { x: int } fun g(v: V) { v.x = 5; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'v'")),
        "{d:?}"
    );
}

#[test]
fn mutation_past_a_ref_boundary_in_a_const_chain_is_allowed() {
    // `b` is a const value struct, but `b.r.v` mutates the shared R
    // object, not `b` itself — allowed. Replacing `b.r` would mutate
    // `b`'s own copy — rejected.
    let src = "refstruct R { v: int }\n\
                   struct Box { r: R }\n\
                   fun f() { const b: Box = Box { r: R { v: 1 } }; b.r.v = 7; }";
    let d = diags(src);
    assert!(d.is_empty(), "unexpected: {d:?}");

    let src = "refstruct R { v: int }\n\
                   struct Box { r: R }\n\
                   fun f() { const b: Box = Box { r: R { v: 1 } }; b.r = R { v: 2 }; }";
    let d = diags(src);
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'b'")),
        "{d:?}"
    );
}

#[test]
fn refstruct_equality_is_well_typed() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(a: P, b: P): bool { return a == b || a != b; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn optional_annotation_accepts_null_and_values() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f() { var p: P? = null; p = P { x: 1 }; p = null; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn bare_null_needs_an_annotation() {
    let d = diags("fun f() { var x = null; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("missing type annotation") && e.help.is_some()),
        "{d:?}"
    );
}

#[test]
fn plain_dot_on_an_optional_errors_with_a_hint() {
    let d = diags("refstruct P { x: int } fun f(p: P?): int { return p.x; }");
    assert!(
        d.iter().any(|e| e.message.contains("may be null")
            && e.help.as_deref().is_some_and(|h| h.contains("?."))),
        "{d:?}"
    );
}

#[test]
fn optional_chaining_produces_an_optional() {
    let d = diags("refstruct P { x: int } fun f(p: P?): int? { return p?.x; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn optional_chaining_on_a_non_optional_is_rejected() {
    let d = diags("refstruct P { x: int } fun f(p: P): int? { return p?.x; }");
    assert!(d.iter().any(|e| e.message.contains("never null")), "{d:?}");
}

#[test]
fn coalescing_unwraps_an_optional() {
    let d = diags("refstruct P { x: int } fun f(p: P?): int { return p?.x ?? 0; }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn coalescing_requires_an_optional_left_side() {
    let d = diags("fun f(a: int): int { return a ?? 1; }");
    assert!(d.iter().any(|e| e.message.contains("'??'")), "{d:?}");
}

#[test]
fn null_checks_narrow_in_if_and_while() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun sum(head: Node?): int {\n\
                 var acc: int = 0;\n\
                 var cur: Node? = head;\n\
                 while cur != null { acc = acc + cur.v; cur = cur.next; }\n\
                 if head != null { acc = acc + head.v; }\n\
                 return acc;\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn leading_null_check_narrows_the_rest_of_the_condition() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?): bool { return p != null && p.x > 0; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn equals_null_narrows_the_else_branch() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?): int { if p == null { return 0; } else { return p.x; } }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn null_comparison_on_a_non_optional_is_rejected() {
    let d = diags("fun f(a: int): bool { return a == null; }");
    assert!(
        d.iter().any(|e| e.message.contains("cannot apply '=='")),
        "{d:?}"
    );
}

#[test]
fn values_fit_optional_parameters() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?) { }\n\
             fun g() { f(P { x: 1 }); f(null); }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn struct_literal_fields_accept_null_for_optionals() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f() { const n: Node = Node { v: 1, next: null }; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

// --- Narrowing edges: what v1 must NOT accept, pinned deliberately ---

#[test]
fn or_conditions_do_not_narrow() {
    // If the left of `||` is false, p IS null — narrowing the right
    // side would be unsound.
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?): bool { return p != null || p.x > 0; }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn early_return_narrows_after_the_if() {
    // The v1 ceiling lifted by ADR 0020: a guard that never falls
    // through leaves its negation facts behind.
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?): int { if p == null { return 0; } return p.x; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn guard_return_narrows_locals() {
    // ADR 0033: join facts reach a local bound in the same frame — the
    // bind-guard-use shape works for params and locals alike.
    let d = diags(
        "refstruct P { x: int }\n\
         fun get(): P? { return null; }\n\
         fun f(): int {\n\
             var p: P? = get();\n\
             if p == null { return 0; }\n\
             return p.x;\n\
         }\n\
         fun g(): int {\n\
             var n: int? = null;\n\
             if n == null { return 0; }\n\
             return n + 1;\n\
         }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn guard_return_narrows_locals_in_nested_blocks() {
    let d = diags(
        "refstruct P { x: int }\n\
         fun get(): P? { return null; }\n\
         fun f(b: bool): int {\n\
             if b {\n\
                 var p: P? = get();\n\
                 if p == null { return 1; }\n\
                 return p.x;\n\
             }\n\
             return 0;\n\
         }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn guarded_local_unnarrows_on_reassignment() {
    let d = diags(
        "refstruct P { x: int }\n\
         fun get(): P? { return null; }\n\
         fun f(): int {\n\
             var p: P? = get();\n\
             if p == null { return 0; }\n\
             p = get();\n\
             return p.x;\n\
         }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn rebinding_after_a_local_guard_unnarrows() {
    // The outer guard's fact must not leak to a new inner binding; the
    // outer binding stays narrowed after the inner scope dies.
    let d = diags(
        "refstruct P { x: int }\n\
         fun get(): P? { return null; }\n\
         fun f(b: bool): int {\n\
             var p: P? = get();\n\
             if p == null { return 0; }\n\
             if b { const p: P? = get(); return p.x; }\n\
             return p.x;\n\
         }",
    );
    assert_eq!(d.len(), 1, "{d:?}");
    assert!(d[0].message.contains("may be null"), "{d:?}");
}

#[test]
fn reassignment_inside_a_narrowed_block_unnarrows() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(q: P?): int {\n\
                 var p: P? = q;\n\
                 if p != null { p = null; return p.x; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn shadowing_inside_a_narrowed_block_unnarrows() {
    let d = diags(
        "refstruct P { x: int }\n\
             fun get(): P? { return null; }\n\
             fun f(p: P?): int {\n\
                 if p != null { const p: P? = get(); return p.x; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn while_conditions_narrow_through_extra_checks() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(head: Node?): int {\n\
                 var cur: Node? = head;\n\
                 var n: int = 0;\n\
                 while cur != null && cur.v < 5 { n = n + 1; cur = cur.next; }\n\
                 return n;\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn narrowed_value_type_optionals_support_arithmetic() {
    let d = diags("fun f(x: int?): int { if x != null { return x + 1; } else { return 0; } }");
    assert!(d.is_empty(), "unexpected: {d:?}");
}

// --- Field-path narrowing ---

#[test]
fn field_null_checks_narrow_in_loops() {
    let d = diags(
        "refstruct Tree { v: int, left: Tree? }\n\
             fun min(t: Tree): int {\n\
                 var cur: Tree = t;\n\
                 while cur.left != null { cur = cur.left; }\n\
                 return cur.v;\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn field_null_checks_narrow_reads_through_the_chain() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node): int {\n\
                 if n.next != null { return n.next.v; } else { return 0; }\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn assigning_to_a_narrowed_field_unnarrows_it() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node): int {\n\
                 if n.next != null { n.next = null; return n.next.v; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn calls_unnarrow_field_paths() {
    // The callee can reach the same object through the shared ref.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun touch(n: Node) { n.next = null; }\n\
             fun f(n: Node): int {\n\
                 if n.next != null { touch(n); return n.next.v; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn writes_through_other_paths_unnarrow_field_facts() {
    // b may alias a (refstructs), so a.next can't stay narrowed.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(a: Node, b: Node): int {\n\
                 if a.next != null { b.next = null; return a.next.v; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn rebinding_the_root_unnarrows_its_field_facts() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(a: Node, b: Node): int {\n\
                 var cur: Node = a;\n\
                 if cur.next != null { cur = b; return cur.next.v; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn facts_do_not_survive_calls_later_in_the_condition() {
    // kill() runs after the null check and can null the field through
    // the shared ref — the body must not trust the fact.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun kill(n: Node): bool { n.next = null; return true; }\n\
             fun f(a: Node): int {\n\
                 if a.next != null && kill(a) { return a.next.v; }\n\
                 return 0;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn outer_facts_invalidated_inside_loop_bodies_do_not_leak() {
    // Sound on iteration 1, null on iteration 2 — the outer fact must
    // be dropped on loop entry because the body assigns its place.
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(q: P?): int {\n\
                 var p: P? = q;\n\
                 var i: int = 0;\n\
                 var sum: int = 0;\n\
                 if p != null {\n\
                     while i < 2 { sum = sum + p.x; p = null; i = i + 1; }\n\
                 }\n\
                 return sum;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn outer_field_facts_killed_by_calls_in_loop_bodies_do_not_leak() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun kill(n: Node) { n.next = null; }\n\
             fun f(a: Node): int {\n\
                 var i: int = 0;\n\
                 var sum: int = 0;\n\
                 if a.next != null {\n\
                     while i < 2 { sum = sum + a.next.v; kill(a); i = i + 1; }\n\
                 }\n\
                 return sum;\n\
             }",
    );
    assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
}

#[test]
fn guarded_deep_chain_assignment_is_allowed() {
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node) {\n\
                 if n.next != null && n.next.next != null { n.next.next.v = 1; }\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn guarded_optional_ref_field_write_through_const_is_allowed() {
    // ADR 0006: the write goes through the shared R object, so const
    // `b` is fine — the guard narrows b.r across the ref boundary.
    let d = diags(
        "refstruct R { v: int }\n\
             struct Box { r: R? }\n\
             fun f() {\n\
                 const b: Box = Box { r: R { v: 1 } };\n\
                 if b.r != null { b.r.v = 2; }\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn shadowing_in_an_inner_block_restores_narrowing_after_it() {
    // The inner `const p` hides the fact only while its scope lives;
    // the outer p at p.x is still the proven-non-null binding.
    let d = diags(
        "refstruct P { x: int }\n\
             fun f(p: P?, b: bool): int {\n\
                 if p != null {\n\
                     if b { const p: int = 1; }\n\
                     return p.x;\n\
                 }\n\
                 return 0;\n\
             }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

// --- Optional-chaining edges ---

#[test]
fn optional_chaining_flattens_already_optional_fields() {
    // n?.next is Node?, not Node?? — there is no double optional.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node?): Node? { return n?.next; }",
    );
    assert!(d.is_empty(), "unexpected: {d:?}");
}

#[test]
fn plain_dot_into_an_optional_field_errors_with_a_hint() {
    // n.next is fine to read, but chaining `.v` through it is not.
    let d = diags(
        "refstruct Node { v: int, next: Node? }\n\
             fun f(n: Node): int { return n.next.v; }",
    );
    assert!(
        d.iter().any(|e| e.message.contains("may be null")
            && e.help.as_deref().is_some_and(|h| h.contains("?."))),
        "{d:?}"
    );
}

#[test]
fn coalescing_an_already_unwrapped_value_is_rejected() {
    // (a ?? 1) is int; a second ?? has nothing left to unwrap.
    let d = diags("fun f(a: int?): int { return a ?? 1 ?? 9; }");
    assert!(
        d.iter().any(|e| e.message.contains("cannot apply '??'")),
        "{d:?}"
    );
}

#[test]
fn assigning_to_const_is_an_error() {
    let d = diags("fun f(): int { const x: int = 1; x = 2; return x; }");
    assert!(
        d.iter()
            .any(|e| e.message.contains("cannot assign to const 'x'")),
        "{d:?}"
    );
}
