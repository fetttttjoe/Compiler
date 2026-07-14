//! Null-narrowing analysis over the AST: which place paths a condition
//! proves non-null, and what a loop body or call can invalidate. Pure
//! syntax — this module never sees `Type`; the checker owns the stateful
//! fact stack and consumes these primitives.

use std::collections::HashSet;

use crate::ast::{BinOp, Expr, Stmt};

/// One narrowing region: facts proven non-null on entry, plus places whose
/// facts are hidden for the region's duration because a new binding shadows
/// the name — hiding ends automatically when the frame pops.
pub(crate) struct NarrowFrame {
    pub(crate) facts: HashSet<String>,
    pub(crate) shadowed: HashSet<String>,
}

impl NarrowFrame {
    pub(crate) fn new(facts: HashSet<String>) -> NarrowFrame {
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

/// The place paths a condition proves non-null: `(if_true, if_false)`.
/// `x != null` (or `x.f != null`) proves the path in the true branch,
/// `== null` in the false branch; `&&` accumulates its sides' true-facts.
pub(crate) fn null_checks(cond: &Expr) -> (HashSet<String>, HashSet<String>) {
    let (mut if_true, mut if_false) = (HashSet::new(), HashSet::new());
    if let Expr::Binary { op, lhs, rhs, .. } = cond {
        match op {
            BinOp::Ne => if_true.extend(place_vs_null(lhs, rhs)),
            BinOp::Eq => if_false.extend(place_vs_null(lhs, rhs)),
            BinOp::And => {
                let (mut lhs_true, _) = null_checks(lhs);
                // The right side runs after the left's checks — a call in
                // it can null a checked field through an alias, so field
                // facts don't cross it. (Bare-variable facts survive; a
                // callee can't rebind the caller's locals.)
                if contains_call(rhs) {
                    lhs_true.retain(|p| !p.contains('.'));
                }
                if_true.extend(lhs_true);
                if_true.extend(null_checks(rhs).0);
            }
            _ => {}
        }
    }
    (if_true, if_false)
}

/// Does this expression contain a call? Calls can mutate any shared
/// refstruct they can reach, which kills field-path narrowing facts.
pub(crate) fn contains_call(e: &Expr) -> bool {
    match e {
        Expr::Call { .. } => true,
        Expr::Unary { rhs, .. } => contains_call(rhs),
        Expr::Binary { lhs, rhs, .. } => contains_call(lhs) || contains_call(rhs),
        Expr::Field { base, .. } => contains_call(base),
        Expr::StructLit { fields, .. } => fields.iter().any(|(_, v)| contains_call(v)),
        Expr::ArrayLit { elements, .. } => elements.iter().any(contains_call),
        Expr::Index { base, index, .. } => contains_call(base) || contains_call(index),
        Expr::Int(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::Str(..)
        | Expr::Ident(..)
        | Expr::Null(_) => false,
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
        }
    }
}

pub(crate) fn place_vs_null(a: &Expr, b: &Expr) -> Option<String> {
    match (a, b) {
        (Expr::Null(_), e) | (e, Expr::Null(_)) => e.place_path(),
        _ => None,
    }
}
