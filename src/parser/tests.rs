//! Parser unit tests: shape assertions over the s-expression rendering,
//! recovery behavior, and the safety budgets.

use super::*;
use crate::lexer::lex;

/// Parse a single expression from source (used to drive the Pratt tests).
fn expr(src: &str) -> Expr {
    let (tokens, diags) = lex(src);
    assert!(diags.is_empty(), "lex errors: {diags:?}");
    let mut p = Parser::new(&tokens);
    let e = p.parse_expr(0);
    assert!(
        p.diagnostics.is_empty(),
        "parse errors: {:?}",
        p.diagnostics
    );
    e
}

#[test]
fn product_binds_tighter_than_sum() {
    assert_eq!(expr("a - b * c").sexpr(), "(- a (* b c))");
}

#[test]
fn sum_is_left_associative() {
    assert_eq!(expr("a - b - c").sexpr(), "(- (- a b) c)");
}

#[test]
fn unary_binds_tighter_than_product() {
    assert_eq!(expr("-a * b").sexpr(), "(* (- a) b)");
}

#[test]
fn parentheses_override_precedence() {
    assert_eq!(expr("(a - b) * c").sexpr(), "(* (- a b) c)");
}

#[test]
fn function_call() {
    assert_eq!(expr("f(a, b)").sexpr(), "(call f a b)");
}

#[test]
fn conversion_calls_parse_as_their_own_node() {
    // ADR 0028/0029: int/float/string are keywords, so this is not a Call.
    assert_eq!(expr("int(x + 1.5)").sexpr(), "(int (+ x 1.5))");
    assert_eq!(expr("float(3) / 2.0").sexpr(), "(/ (float 3) 2)");
    assert_eq!(
        expr("string(x) + string(1)").sexpr(),
        "(+ (string x) (string 1))"
    );
}

#[test]
fn template_literals_desugar_to_concat() {
    // ADR 0030: text parts are string literals, `${e}` an implicit
    // string(e), the whole a left fold over `+`.
    assert_eq!(
        expr("`a ${x} b`").sexpr(),
        r#"(+ (+ "a " (string x)) " b")"#
    );
    assert_eq!(expr("`${x}${y}`").sexpr(), "(+ (string x) (string y))");
    assert_eq!(expr("``").sexpr(), "\"\"");
    assert_eq!(
        expr("`${`i${x}`}!`").sexpr(),
        r#"(+ (string (+ "i" (string x))) "!")"#
    );
}

#[test]
fn conversion_without_parens_is_an_error() {
    let (tokens, _) = lex("fun main(): int { return int; }");
    let (_, diags) = parse(&tokens);
    assert!(!diags.is_empty());
}

#[test]
fn field_access_chains_left() {
    assert_eq!(expr("a.b.c").sexpr(), "(. (. a b) c)");
}

#[test]
fn call_then_field_access() {
    assert_eq!(expr("f(x).y").sexpr(), "(. (call f x) y)");
}

#[test]
fn struct_literal() {
    assert_eq!(
        expr("Point { x: 1, y: 2 }").sexpr(),
        "(struct Point x=1 y=2)"
    );
}

fn stmt(src: &str) -> Stmt {
    let (tokens, diags) = lex(src);
    assert!(diags.is_empty(), "lex errors: {diags:?}");
    let mut p = Parser::new(&tokens);
    let (s, _clean) = p.parse_stmt();
    assert!(
        p.diagnostics.is_empty(),
        "parse errors: {:?}",
        p.diagnostics
    );
    s
}

#[test]
fn const_binding_is_immutable() {
    match stmt("const x: int = a + 1;") {
        Stmt::Let {
            mutable,
            name,
            value,
            ..
        } => {
            assert!(!mutable);
            assert_eq!(name, "x");
            assert_eq!(value.sexpr(), "(+ a 1)");
        }
        other => panic!("expected Let, got {other:?}"),
    }
}

#[test]
fn var_binding_is_mutable() {
    match stmt("var y: int = 2;") {
        Stmt::Let { mutable, .. } => assert!(mutable),
        other => panic!("expected Let, got {other:?}"),
    }
}

#[test]
fn return_with_and_without_value() {
    match stmt("return a * b;") {
        Stmt::Return { value: Some(e), .. } => assert_eq!(e.sexpr(), "(* a b)"),
        other => panic!("expected Return, got {other:?}"),
    }
    match stmt("return;") {
        Stmt::Return { value: None, .. } => {}
        other => panic!("expected empty Return, got {other:?}"),
    }
}

#[test]
fn assignment_statement() {
    match stmt("x = 5;") {
        Stmt::Assign { target, value, .. } => {
            assert_eq!(target.sexpr(), "x");
            assert_eq!(value.sexpr(), "5");
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn field_assignment_statement() {
    match stmt("o.i.v = 1;") {
        Stmt::Assign { target, value, .. } => {
            assert_eq!(target.sexpr(), "(. (. o i) v)");
            assert_eq!(value.sexpr(), "1");
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn optional_chaining_and_coalescing_parse() {
    assert_eq!(expr("a?.b ?? c").sexpr(), "(?? (?. a b) c)");
    assert_eq!(expr("a.b?.c").sexpr(), "(?. (. a b) c)");
}

#[test]
fn coalescing_binds_loosest() {
    assert_eq!(expr("a ?? b || c").sexpr(), "(?? a (|| b c))");
}

#[test]
fn optional_type_annotations_parse() {
    let (tokens, _) = lex("fun f(p: Point?): int? { return 1; }");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    let Item::Function(f) = &ast[0] else { panic!() };
    assert_eq!(
        f.params[0].ty,
        TypeAnn::Optional(Box::new(TypeAnn::Named("Point".into())))
    );
    assert_eq!(
        f.return_type,
        Some(TypeAnn::Optional(Box::new(TypeAnn::Int)))
    );
}

#[test]
fn let_bindings_take_optional_annotations() {
    match stmt("var head: Node? = null;") {
        Stmt::Let { ty, value, .. } => {
            assert_eq!(
                ty,
                Some(TypeAnn::Optional(Box::new(TypeAnn::Named("Node".into()))))
            );
            assert_eq!(value.sexpr(), "null");
        }
        other => panic!("expected Let, got {other:?}"),
    }
}

#[test]
fn array_types_and_literals_parse() {
    match stmt("var xs: int[] = [1, 2, 3];") {
        Stmt::Let { ty, value, .. } => {
            assert_eq!(ty, Some(TypeAnn::Array(Box::new(TypeAnn::Int))));
            assert_eq!(value.sexpr(), "[1 2 3]");
        }
        other => panic!("expected Let, got {other:?}"),
    }
    // Suffixes compose: array of optionals vs optional array.
    let (tokens, _) = lex("fun f(a: int?[], b: int[]?) { }");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    let Item::Function(f) = &ast[0] else { panic!() };
    assert_eq!(
        f.params[0].ty,
        TypeAnn::Array(Box::new(TypeAnn::Optional(Box::new(TypeAnn::Int))))
    );
    assert_eq!(
        f.params[1].ty,
        TypeAnn::Optional(Box::new(TypeAnn::Array(Box::new(TypeAnn::Int))))
    );
}

#[test]
fn indexing_parses_as_postfix() {
    assert_eq!(expr("a[i + 1]").sexpr(), "(idx a (+ i 1))");
    assert_eq!(expr("a[0][1]").sexpr(), "(idx (idx a 0) 1)");
    assert_eq!(expr("xs[0].f").sexpr(), "(. (idx xs 0) f)");
}

#[test]
fn for_loops_parse() {
    match stmt("for x in xs { print(x); }") {
        Stmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            assert_eq!(name, "x");
            assert_eq!(iterable.sexpr(), "xs");
            assert_eq!(body.len(), 1);
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn for_loops_can_bind_the_index() {
    match stmt("for [i, s] in scores { print(s); }") {
        Stmt::For {
            index,
            name,
            iterable,
            ..
        } => {
            assert_eq!(index.as_deref(), Some("i"));
            assert_eq!(name, "s");
            assert_eq!(iterable.sexpr(), "scores");
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn malformed_for_headers_report_once() {
    let (tokens, _) = lex("fun f(xs: int[]) { for [1, x] in xs { } }");
    let (_, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "{pd:?}");
}

#[test]
fn index_targets_are_places() {
    match stmt("a[0] = 5;") {
        Stmt::Assign { target, value, .. } => {
            assert_eq!(target.sexpr(), "(idx a 0)");
            assert_eq!(value.sexpr(), "5");
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn nested_optionals_get_a_dedicated_error() {
    let (tokens, _) = lex("fun f(x: int??) { }");
    let (_, pd) = parse(&tokens);
    assert!(
        pd.iter().any(|e| e.message.contains("nested optionals")),
        "{pd:?}"
    );
    // However many question marks, one mistake reports once.
    let (tokens, _) = lex("fun f(x: int???) { }");
    let (_, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "{pd:?}");
}

#[test]
fn optional_chain_is_not_a_place() {
    let (tokens, _) = lex("a?.b = 1;");
    let mut p = Parser::new(&tokens);
    p.parse_stmt();
    assert!(
        p.diagnostics
            .iter()
            .any(|e| e.message.contains("invalid assignment target")),
        "{:?}",
        p.diagnostics
    );
}

#[test]
fn non_place_assignment_target_is_an_error() {
    let (tokens, _) = lex("f() = 1;");
    let mut p = Parser::new(&tokens);
    p.parse_stmt();
    assert!(
        p.diagnostics
            .iter()
            .any(|e| e.message.contains("invalid assignment target")),
        "{:?}",
        p.diagnostics
    );
}

#[test]
fn expression_statement() {
    match stmt("f(x);") {
        Stmt::Expr(e) => assert_eq!(e.sexpr(), "(call f x)"),
        other => panic!("expected Expr statement, got {other:?}"),
    }
}

#[test]
fn parses_a_function() {
    let (tokens, ld) = lex("fun add(a: int, b: int): int { return a + b; }");
    assert!(ld.is_empty());
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    assert_eq!(ast.len(), 1);
    match &ast[0] {
        Item::Function(f) => {
            assert_eq!(f.name, "add");
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.return_type, Some(TypeAnn::Int));
            assert_eq!(f.body.len(), 1);
        }
        other => panic!("expected function, got {other:?}"),
    }
}

#[test]
fn refstruct_sets_the_by_ref_flag() {
    let (tokens, _) =
        lex("refstruct P { x: int } export refstruct Q { y: int } struct V { z: int }");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    let Item::Struct(p) = &ast[0] else { panic!() };
    assert!(p.by_ref && !p.exported);
    let Item::Struct(q) = &ast[1] else { panic!() };
    assert!(q.by_ref && q.exported);
    let Item::Struct(v) = &ast[2] else { panic!() };
    assert!(!v.by_ref);
}

#[test]
fn parses_a_struct() {
    let (tokens, _) = lex("struct Point { x: int, y: float }");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    match &ast[0] {
        Item::Struct(s) => {
            assert_eq!(s.name, "Point");
            assert_eq!(s.fields.len(), 2);
            assert_eq!(s.fields[1].ty, TypeAnn::Float);
        }
        other => panic!("expected struct, got {other:?}"),
    }
}

#[test]
fn parses_an_import_declaration() {
    let (tokens, ld) = lex("import { fib, add } from \"./math\";");
    assert!(ld.is_empty(), "{ld:?}");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    let Item::Import(imp) = &ast[0] else {
        panic!("expected import")
    };
    let names: Vec<&str> = imp.names.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["fib", "add"]);
    assert_eq!(imp.path, "./math");
}

#[test]
fn export_marks_functions_and_structs() {
    let (tokens, _) =
        lex("export fun f(): int { return 1; } export struct P { x: int } fun g() { }");
    let (ast, pd) = parse(&tokens);
    assert!(pd.is_empty(), "parse errors: {pd:?}");
    let Item::Function(f) = &ast[0] else { panic!() };
    assert!(f.exported);
    let Item::Struct(s) = &ast[1] else { panic!() };
    assert!(s.exported);
    let Item::Function(g) = &ast[2] else { panic!() };
    assert!(!g.exported);
}

#[test]
fn export_before_junk_is_reported_and_recovered() {
    let (tokens, _) = lex("export 42 fun ok(): int { return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "{pd:?}");
    assert!(pd[0].message.contains("after 'export'"), "{pd:?}");
    assert_eq!(ast.len(), 1);
}

#[test]
fn import_with_missing_path_recovers() {
    let (tokens, _) = lex("import { f } from ; fun ok(): int { return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "{pd:?}");
    assert!(pd[0].message.contains("module path"), "{pd:?}");
    assert_eq!(ast.len(), 2); // the (broken) import + the function
}

#[test]
fn recovers_from_a_bad_top_level_token() {
    // A stray token is reported, then parsing resumes at `fun`.
    let (tokens, _) = lex("42 fun ok(): int { return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1);
    assert_eq!(ast.len(), 1);
}

#[test]
fn recovers_from_a_stray_top_level_right_brace() {
    // Regression: `synchronize()` used to return without consuming a
    // stray `}`, leaving `parse()` stuck on the same token forever.
    let (tokens, _) = lex("} fun ok(): int { return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1);
    assert_eq!(ast.len(), 1);
}

#[test]
fn equality_binds_looser_than_comparison_tighter_than_logic() {
    assert_eq!(expr("a == b && c < d").sexpr(), "(&& (== a b) (< c d))");
    assert_eq!(expr("a < b == c >= d").sexpr(), "(== (< a b) (>= c d))");
}

#[test]
fn bool_and_string_atoms() {
    assert_eq!(expr("true").sexpr(), "true");
    assert_eq!(expr("!false").sexpr(), "(! false)");
    assert_eq!(expr("\"hi\" + \"!\"").sexpr(), "(+ \"hi\" \"!\")");
}

#[test]
fn if_with_else_if_chain() {
    let s = stmt("if a { return 1; } else if b { return 2; } else { return 3; }");
    let Stmt::If {
        cond,
        then_body,
        else_body,
        ..
    } = s
    else {
        panic!("expected If, got something else");
    };
    assert_eq!(cond.sexpr(), "a");
    assert_eq!(then_body.len(), 1);
    // The `else if` is a single nested If in the else body.
    let else_body = else_body.expect("else body");
    assert_eq!(else_body.len(), 1);
    let Stmt::If {
        cond: nested_cond,
        else_body: nested_else,
        ..
    } = &else_body[0]
    else {
        panic!("expected nested If in else body");
    };
    assert_eq!(nested_cond.sexpr(), "b");
    assert!(nested_else.is_some());
}

#[test]
fn while_statement() {
    let s = stmt("while i < n { i = i + 1; }");
    let Stmt::While { cond, body, .. } = s else {
        panic!("expected While");
    };
    assert_eq!(cond.sexpr(), "(< i n)");
    assert_eq!(body.len(), 1);
}

#[test]
fn break_and_continue_parse_as_statements() {
    assert!(matches!(stmt("break;"), Stmt::Break { .. }));
    assert!(matches!(stmt("continue;"), Stmt::Continue { .. }));
    let s = stmt("while true { break; continue; }");
    let Stmt::While { body, .. } = s else {
        panic!("expected While");
    };
    assert!(matches!(body[0], Stmt::Break { .. }));
    assert!(matches!(body[1], Stmt::Continue { .. }));
}

#[test]
fn recovery_stops_before_break_and_continue() {
    // A malformed statement synchronizes at the keywords instead of
    // eating them, so the loop controls survive the recovery.
    let (tokens, _) = lex("fun f(): int { while true { const = 1 break; } return 7; }");
    let (ast, pd) = parse(&tokens);
    assert!(!pd.is_empty());
    let Item::Function(f) = &ast[0] else {
        panic!("expected function")
    };
    let Some(Stmt::While { body, .. }) = f.body.first() else {
        panic!("expected while")
    };
    assert!(
        body.iter().any(|s| matches!(s, Stmt::Break { .. })),
        "break must survive recovery: {body:?}"
    );
}

#[test]
fn condition_is_never_a_struct_literal() {
    // `if x { … }` must read `x` as an identifier condition, not the
    // struct literal `x { }`.
    let s = stmt("if x { return 1; }");
    let Stmt::If { cond, .. } = s else {
        panic!("expected If")
    };
    assert_eq!(cond.sexpr(), "x");
}

#[test]
fn parenthesized_condition_allows_struct_literals_again() {
    let s = stmt("if (P { x: 1 }).x == 1 { return 1; }");
    let Stmt::If { cond, .. } = s else {
        panic!("expected If")
    };
    assert_eq!(cond.sexpr(), "(== (. (struct P x=1) x) 1)");
}

#[test]
fn nested_block_error_does_not_eat_the_following_statement() {
    // Regression: the outer block used to see the nested block's (already
    // recovered) diagnostic and synchronize again, swallowing `x = 5;`.
    let (tokens, _) =
        lex("fun f(): int { var x: int = 0; if true { const = 1; } x = 5; return x; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "{pd:?}");
    let Item::Function(f) = &ast[0] else {
        panic!("expected function")
    };
    assert_eq!(f.body.len(), 4, "x = 5; must survive: {:?}", f.body);
    assert!(matches!(f.body[2], Stmt::Assign { .. }));
}

#[test]
fn struct_literal_allowed_in_call_arguments_inside_condition() {
    let s = stmt("if eq(P { x: 1 }) { return 1; }");
    let Stmt::If { cond, .. } = s else {
        panic!("expected If")
    };
    assert_eq!(cond.sexpr(), "(call eq (struct P x=1))");
}

#[test]
fn missing_semicolon_after_struct_literal_does_not_cascade() {
    // Regression: the statement ends in the struct literal's `}`, which
    // used to spoof the "ended cleanly" check and skip recovery.
    let (tokens, _) = lex("fun f(): int { const p: P = P { x: 1 } ) return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(pd.len(), 1, "one missing-semicolon error only: {pd:?}");
    let Item::Function(f) = &ast[0] else {
        panic!("expected function")
    };
    assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
}

#[test]
fn error_placeholder_consuming_a_semicolon_does_not_cascade() {
    // Regression: `var x = ;` — parse_atom's recovery consumed the `;`,
    // which used to spoof the clean-end check; the junk after it then
    // parsed as phantom statements.
    let (tokens, _) = lex("fun f(): int { var x = ; y z w return 1; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(
        pd.len(),
        2,
        "missing expression + missing semicolon: {pd:?}"
    );
    let Item::Function(f) = &ast[0] else {
        panic!("expected function")
    };
    assert_eq!(f.body.len(), 2, "no phantom statements: {:?}", f.body);
    assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
}

#[test]
fn pathological_nesting_errors_instead_of_overflowing_the_stack() {
    // 500 nested parens.
    let src = format!(
        "fun f(): int {{ return {}1{}; }}",
        "(".repeat(500),
        ")".repeat(500)
    );
    let (tokens, _) = lex(&src);
    let (_, pd) = parse(&tokens);
    assert!(
        pd.iter().any(|e| e.message.contains("nesting exceeds")),
        "expected a nesting diagnostic: {} diags",
        pd.len()
    );

    // A 500-deep `else if` chain.
    let mut src = String::from("fun f(a: bool): int { if a { return 1; }");
    for _ in 0..500 {
        src.push_str(" else if a { return 1; }");
    }
    src.push_str(" else { return 0; } }");
    let (tokens, _) = lex(&src);
    let (_, pd) = parse(&tokens);
    assert!(
        pd.iter().any(|e| e.message.contains("nesting exceeds")),
        "expected a nesting diagnostic: {} diags",
        pd.len()
    );
}

#[test]
fn malformed_statement_recovers_at_statement_boundary() {
    // `const = 1 2 3;` is broken; recovery skips to the `;` so the
    // following statement parses cleanly instead of cascading.
    let (tokens, _) = lex("fun f(): int { const = 1 2 3; return 7; }");
    let (ast, pd) = parse(&tokens);
    assert_eq!(
        pd.len(),
        2,
        "one missing-name error + one at the `2`: {pd:?}"
    );
    let Item::Function(f) = &ast[0] else {
        panic!("expected function")
    };
    assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
}

/// Budget tests build trees up to MAX_FN_OPS tall; run them on a
/// stack that comfortably fits the drop glue (test threads get 2MB).
fn on_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

fn budget_diags(src: &str) -> Vec<Diagnostic> {
    let (tokens, lex_diags) = lex(src);
    assert!(lex_diags.is_empty(), "lex errors: {lex_diags:?}");
    let (ast, diags) = parse(&tokens);
    drop(ast); // dropping the frozen tree must not overflow either
    diags
}

#[test]
fn operator_chains_charge_the_function_budget() {
    on_big_stack(|| {
        let chain = vec!["1"; MAX_FN_OPS as usize + 10].join(" + ");
        let diags = budget_diags(&format!("fun f(): int {{ return {chain}; }}"));
        let hits = diags
            .iter()
            .filter(|d| d.message.contains("operators"))
            .count();
        assert_eq!(hits, 1, "reported once per function: {}", diags.len());
    });
}

#[test]
fn field_chains_charge_the_same_budget() {
    on_big_stack(|| {
        let links = ".f".repeat(MAX_FN_OPS as usize + 10);
        let diags = budget_diags(&format!("fun f(): int {{ return x{links}; }}"));
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.message.contains("operators"))
                .count(),
            1
        );
    });
}

#[test]
fn operator_budget_resets_between_functions() {
    on_big_stack(|| {
        // Each function is under the cap; only a leaked counter would trip.
        let chain = vec!["1"; 20_000].join(" + ");
        let src = format!("fun a(): int {{ return {chain}; }}\nfun b(): int {{ return {chain}; }}");
        assert!(budget_diags(&src).is_empty());
    });
}
