//! Narrowing analysis over the AST: which place paths a condition
//! proves non-null or (not) in the error state, and what a loop body or
//! call can invalidate. Pure syntax — this module never sees `Type`;
//! the checker owns the stateful fact stack and consumes these
//! primitives.

use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Expr, Stmt};

/// What a condition proved about a place path (ADR 0007/0034): `T?`
/// proven present, `T!` proven a value, or `T!` proven an error.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Fact {
    NonNull,
    NoErr,
    IsErr,
}

/// One narrowing region: facts proven on entry, plus places whose
/// facts are hidden for the region's duration because a new binding shadows
/// the name — hiding ends automatically when the frame pops.
#[derive(Clone)]
pub(crate) struct NarrowFrame {
    pub(crate) facts: HashMap<String, Fact>,
    pub(crate) shadowed: HashSet<String>,
}

impl NarrowFrame {
    pub(crate) fn new(facts: HashMap<String, Fact>) -> NarrowFrame {
        NarrowFrame {
            facts,
            shadowed: HashSet::new(),
        }
    }
}

/// Is `path` the same place as `prefix`, or reached through it
/// (`covers("cur", "cur.left")` is true; `covers("cur", "curx")` is not)?
pub(crate) fn covers(prefix: &str, path: &str) -> bool {
    path == prefix || (path.starts_with(prefix) && path[prefix.len()..].starts_with('.'))
}

/// The facts a condition proves: `(if_true, if_false)`. `x != null`
/// proves NonNull in the true branch, `== null` in the false branch;
/// `x == error` proves IsErr true / NoErr false (`!=` the inverse —
/// unlike null, both error branches carry a fact); `&&` accumulates
/// its sides' true-facts.
pub(crate) fn condition_facts(cond: &Expr) -> (HashMap<String, Fact>, HashMap<String, Fact>) {
    let (mut if_true, mut if_false) = (HashMap::new(), HashMap::new());
    if let Expr::Binary { op, lhs, rhs, .. } = cond {
        match op {
            BinOp::Ne => {
                if let Some(p) = place_vs_null(lhs, rhs) {
                    if_true.insert(p, Fact::NonNull);
                }
                if let Some(p) = place_vs_error(lhs, rhs) {
                    if_true.insert(p.clone(), Fact::NoErr);
                    if_false.insert(p, Fact::IsErr);
                }
            }
            BinOp::Eq => {
                if let Some(p) = place_vs_null(lhs, rhs) {
                    if_false.insert(p, Fact::NonNull);
                }
                if let Some(p) = place_vs_error(lhs, rhs) {
                    if_true.insert(p.clone(), Fact::IsErr);
                    if_false.insert(p, Fact::NoErr);
                }
            }
            BinOp::And => {
                let (mut lhs_true, _) = condition_facts(lhs);
                // The right side runs after the left's checks — a call in
                // it can null a checked field through an alias, so field
                // facts don't cross it. (Bare-variable facts survive; a
                // callee can't rebind the caller's locals.)
                if contains_call(rhs) {
                    lhs_true.retain(|p, _| !p.contains('.'));
                }
                if_true.extend(lhs_true);
                if_true.extend(condition_facts(rhs).0);
            }
            _ => {}
        }
    }
    (if_true, if_false)
}

/// Does this statement list never fall through — every path ends in
/// `return`, `break`, or `continue`? Loops never count: a contained
/// `break` targets the loop itself, and `while true` analysis stays out,
/// consistent with definite return (ADR 0020).
pub(crate) fn diverges(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => true,
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => diverges(then_body) && diverges(else_body),
        // A match diverges only with an `else` — this analysis is pure
        // syntax and cannot prove exhaustiveness (ADR 0036).
        Stmt::Match {
            arms,
            else_body: Some(else_body),
            ..
        } => arms.iter().all(|a| diverges(&a.body)) && diverges(else_body),
        _ => false,
    })
}

/// Does this expression contain a call? Calls can mutate any shared
/// refstruct they can reach, which kills field-path narrowing facts.
pub(crate) fn contains_call(e: &Expr) -> bool {
    match e {
        Expr::Call { .. } => true,
        Expr::Unary { rhs, .. } => contains_call(rhs),
        Expr::Convert { arg, .. } => contains_call(arg),
        Expr::Binary { lhs, rhs, .. } => contains_call(lhs) || contains_call(rhs),
        Expr::Field { base, .. } => contains_call(base),
        Expr::StructLit { fields, .. } => fields.iter().any(|(_, v)| contains_call(v)),
        // Construction only evaluates its payloads — but they may call.
        Expr::EnumLit { args, .. } => args.iter().any(contains_call),
        Expr::ArrayLit { elements, .. } => elements.iter().any(contains_call),
        Expr::Index { base, index, .. } => contains_call(base) || contains_call(index),
        Expr::Try { expr, .. } => contains_call(expr),
        Expr::Int(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::Str(..)
        | Expr::Ident(..)
        | Expr::Null(_)
        | Expr::ErrorLit(..)
        | Expr::ErrorKind(_) => false,
    }
}

/// What a loop body can do to enclosing narrowing facts on a later
/// iteration: the places it assigns, and whether it can invalidate field
/// facts at all (a call or a write through a field, both alias-reaching).
pub(crate) fn body_effects(
    stmts: &[Stmt],
    assigned: &mut HashSet<String>,
    kills_fields: &mut bool,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { target, value, .. } => {
                if let Some(path) = target.place_path() {
                    assigned.insert(path);
                }
                if !matches!(target, Expr::Ident(..)) || contains_call(value) {
                    *kills_fields = true;
                }
            }
            Stmt::Let { value, .. } | Stmt::Expr(value) => {
                if contains_call(value) {
                    *kills_fields = true;
                }
            }
            Stmt::Return { value, .. } => {
                if value.as_ref().is_some_and(contains_call) {
                    *kills_fields = true;
                }
            }
            // No expressions, no writes — inert for narrowing (ADR 0019).
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                if contains_call(cond) {
                    *kills_fields = true;
                }
                body_effects(then_body, assigned, kills_fields);
                if let Some(else_body) = else_body {
                    body_effects(else_body, assigned, kills_fields);
                }
            }
            Stmt::While { cond, body, .. } => {
                if contains_call(cond) {
                    *kills_fields = true;
                }
                body_effects(body, assigned, kills_fields);
            }
            Stmt::For { iterable, body, .. } => {
                if contains_call(iterable) {
                    *kills_fields = true;
                }
                body_effects(body, assigned, kills_fields);
            }
            Stmt::Match {
                scrutinee,
                arms,
                else_body,
                ..
            } => {
                if contains_call(scrutinee) {
                    *kills_fields = true;
                }
                for arm in arms {
                    body_effects(&arm.body, assigned, kills_fields);
                }
                if let Some(else_body) = else_body {
                    body_effects(else_body, assigned, kills_fields);
                }
            }
        }
    }
}

pub(crate) fn place_vs_null(a: &Expr, b: &Expr) -> Option<String> {
    match (a, b) {
        (Expr::Null(_), e) | (e, Expr::Null(_)) => e.place_path(),
        _ => None,
    }
}

/// `x == error` / `x != error` — the bare `error` marker against a
/// place path (ADR 0034).
pub(crate) fn place_vs_error(a: &Expr, b: &Expr) -> Option<String> {
    match (a, b) {
        (Expr::ErrorKind(_), e) | (e, Expr::ErrorKind(_)) => e.place_path(),
        _ => None,
    }
}
