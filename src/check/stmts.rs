//! Statement checking and flow-sensitive narrowing: function bodies,
//! bindings, assignment legality (the var-root / refstruct-crossing
//! rule), and the null-narrowing fact stack — facts appear on
//! `!= null` guards and die on any write, call, or rebind that could
//! invalidate them (soundness over convenience, ADR 0007).

use super::*;

impl Checker<'_, '_> {
    pub(super) fn check_function(&mut self, f: &Function) {
        let sig = self.sigs[&(self.module, f.name.clone())].clone();
        self.ret = sig.ret;
        let mut scope = HashMap::new();
        for (param, ty) in f.params.iter().zip(sig.params) {
            scope.insert(param.name.clone(), VarInfo { ty, mutable: false });
        }
        self.scopes.push(scope);
        // The base narrowing frame: guard facts at the function's top level
        // live here, and top-level rebinds shadow into it (ADR 0020).
        self.nonnull.push(NarrowFrame::new(HashMap::new()));
        for stmt in &f.body {
            self.check_stmt(stmt);
        }
        self.nonnull.pop();
        self.scopes.pop();

        if self.ret != Type::Unit && !poisoned(&self.ret) && !always_returns(&f.body) {
            // `pretty` is the identity for source names; instances
            // (ADR 0035) drop their module qualifiers.
            self.error(
                format!(
                    "not all paths in function '{}' return a value",
                    crate::types::pretty(&f.name)
                ),
                f.span,
            );
        }
    }

    /// Type-checks a nested block in its own scope (bindings made inside die
    /// at the closing brace), with a set of place paths proven non-null for
    /// its duration. Returns the facts still standing at the block's end —
    /// the survivors a divergence-aware join may carry past an `if`
    /// (writes inside the block already subtracted themselves, ADR 0020).
    fn check_block_narrowed(
        &mut self,
        stmts: &[Stmt],
        facts: HashMap<String, Fact>,
    ) -> HashMap<String, Fact> {
        self.nonnull.push(NarrowFrame::new(facts));
        self.scopes.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        self.scopes.pop();
        self.nonnull.pop().expect("frame pushed above").facts
    }

    /// Snapshot of the fact stack, taken before a diverging branch: a
    /// branch that never falls through cannot affect what follows, so its
    /// narrowing side effects roll back (ADR 0020). `None` when nothing
    /// could change — branches only remove outer facts, never add.
    fn checkpoint(&self, diverging: bool) -> Option<Vec<NarrowFrame>> {
        (diverging && self.has_facts()).then(|| self.nonnull.clone())
    }

    fn rollback(&mut self, saved: Option<Vec<NarrowFrame>>) {
        if let Some(saved) = saved {
            self.nonnull = saved;
        }
    }

    /// Grants the code after a divergence-aware join its proven facts;
    /// they live in the innermost frame and expire with the enclosing
    /// block. A frame always exists (`check_function` pushes the base).
    fn add_facts(&mut self, facts: HashMap<String, Fact>) {
        if let Some(frame) = self.nonnull.last_mut() {
            frame.facts.extend(facts);
        }
    }

    /// Any narrowing facts at all? The cheap gate for the common,
    /// un-narrowed path — skips fact bookkeeping entirely.
    pub(super) fn has_facts(&self) -> bool {
        self.nonnull.iter().any(|f| !f.facts.is_empty())
    }

    pub(super) fn is_nonnull(&self, path: &str) -> bool {
        self.fact_of(path) == Some(Fact::NonNull)
    }

    /// The innermost standing fact for `path`, if any. Innermost first: a
    /// shadow hides outer facts for its own region. Within a frame, facts
    /// outrank the shadow — a fact can only enter after the frame's shadow
    /// exists, so it is about the shadowing binding itself (`bind` killed
    /// any stale same-frame fact, ADR 0033).
    pub(super) fn fact_of(&self, path: &str) -> Option<Fact> {
        for frame in self.nonnull.iter().rev() {
            if let Some(fact) = frame.facts.get(path) {
                return Some(*fact);
            }
            if frame.shadowed.iter().any(|s| covers(s, path)) {
                return None;
            }
        }
        None
    }

    /// Permanently drops a place path — and everything reached through it —
    /// from every narrowing frame. Used when the place is reassigned.
    pub(super) fn unnarrow(&mut self, path: &str) {
        for frame in &mut self.nonnull {
            frame.facts.retain(|q, _| !covers(path, q));
        }
    }

    /// Facts from outside a loop go stale on iteration 2 if the body can
    /// invalidate them (only the loop's own condition is re-checked each
    /// pass) — drop everything the body can touch before checking it. The
    /// condition's own facts are pushed fresh afterwards and stay safe.
    fn drop_loop_invalidated_facts(&mut self, body: &[Stmt]) {
        if !self.has_facts() {
            return;
        }
        let mut assigned = HashSet::new();
        let mut kills_fields = false;
        body_effects(body, &mut assigned, &mut kills_fields);
        for path in &assigned {
            self.unnarrow(path);
        }
        if kills_fields {
            self.unnarrow_field_paths();
        }
    }

    /// Field-path facts don't survive calls or writes through fields — any
    /// of those can reach the checked object through an alias. Bare
    /// variable facts do survive: a callee can't rebind the caller's locals.
    pub(super) fn unnarrow_field_paths(&mut self) {
        for frame in &mut self.nonnull {
            frame.facts.retain(|q, _| !q.contains('.'));
        }
    }

    /// `match` (ADR 0036): the scrutinee must be an enum; every arm
    /// names a distinct variant and binds its payloads (`_` skips);
    /// coverage is all variants or an `else`. Arms mirror `if`
    /// branches for narrowing — own frame and scope, divergence-aware
    /// rollback.
    fn check_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::MatchArm],
        else_body: Option<&[Stmt]>,
        span: Span,
    ) {
        let s_ty = self.type_of_expr(scrutinee);
        self.unnarrow_field_paths(); // the scrutinee may call
        let def = match &s_ty {
            Type::Enum(m, n) => Some(self.mono.enums[&(*m, n.clone())].clone()),
            t if poisoned(t) => None,
            other => {
                self.error(
                    format!("match needs an enum, found {}", self.type_name(other)),
                    scrutinee.span(),
                );
                None
            }
        };
        let mut seen: HashSet<String> = HashSet::new();
        for arm in arms {
            let payloads = match &def {
                Some(def) => match def.variants.iter().position(|(n, _)| n == &arm.variant) {
                    Some(tag) => {
                        if !seen.insert(arm.variant.clone()) {
                            self.error(
                                format!("duplicate arm for variant '{}'", arm.variant),
                                arm.variant_span,
                            );
                        }
                        self.out.variant_tags.insert(arm.variant_span, tag as u32);
                        let payloads = def.variants[tag].1.clone();
                        if arm.bindings.len() != payloads.len() {
                            self.error(
                                format!(
                                    "variant '{}' has {} payload(s), found {} binding(s)",
                                    arm.variant,
                                    payloads.len(),
                                    arm.bindings.len()
                                ),
                                arm.variant_span,
                            );
                        }
                        payloads
                    }
                    None => {
                        let names = def.variants.iter().map(|(n, _)| n.as_str());
                        self.diagnostics.push(
                            Diagnostic::error(
                                format!("this enum has no variant '{}'", arm.variant),
                                arm.variant_span,
                            )
                            .suggest(&arm.variant, names),
                        );
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            // The arm body: its own frame and scope with the payload
            // bindings; a diverging arm's narrowing side effects roll
            // back, like an if branch (ADR 0020).
            let saved = self.checkpoint(diverges(&arm.body));
            self.nonnull.push(NarrowFrame::new(HashMap::new()));
            self.scopes.push(HashMap::new());
            for (i, (bname, _)) in arm.bindings.iter().enumerate() {
                if bname != "_" {
                    let ty = payloads.get(i).cloned().unwrap_or(Type::Error);
                    self.bind(bname, ty, false);
                }
            }
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.scopes.pop();
            self.nonnull.pop();
            self.rollback(saved);
        }
        if let Some(else_body) = else_body {
            let saved = self.checkpoint(diverges(else_body));
            self.check_block_narrowed(else_body, HashMap::new());
            self.rollback(saved);
        } else if let Some(def) = &def {
            let missing: Vec<&str> = def
                .variants
                .iter()
                .map(|(n, _)| n.as_str())
                .filter(|n| !seen.contains(*n))
                .collect();
            if !missing.is_empty() {
                self.error(
                    format!(
                        "match does not cover variant(s) {} — add arms or 'else'",
                        missing
                            .iter()
                            .map(|n| format!("'{n}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    span,
                );
            }
        }
    }

    pub(super) fn check_condition(&mut self, keyword: &str, cond: &Expr) {
        let ty = self.type_of_expr(cond);
        if !fits(&ty, &Type::Bool) {
            self.error(
                format!(
                    "{keyword} condition must be bool, found {}",
                    self.type_name(&ty)
                ),
                cond.span(),
            );
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let {
                mutable,
                name,
                ty,
                value,
                span,
            } => {
                let ty = match ty {
                    Some(ann) => {
                        let declared = self.resolve(ann, *span);
                        // Codegen gates bindings on this resolved type —
                        // never on the raw annotation.
                        self.out.let_types.insert(*span, declared.clone());
                        if !self.check_literal_against(value, &declared) {
                            let init_ty = self.type_of_rhs(value);
                            if !fits(&init_ty, &declared) {
                                self.error(
                                    format!(
                                        "'{name}' is declared as {} but initialized with {}",
                                        self.type_name(&declared),
                                        self.type_name(&init_ty)
                                    ),
                                    *span,
                                );
                            }
                        }
                        declared
                    }
                    // Every binding declares its type (ADR 0010); the
                    // annotation error leads, then the initializer is still
                    // typed for its own errors.
                    None => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                format!("missing type annotation for '{name}'"),
                                *span,
                            )
                            .with_help(format!(
                                "every binding declares its type: 'var {name}: <type> = …;'"
                            )),
                        );
                        let init_ty = self.type_of_rhs(value);
                        if init_ty == Type::Null || unconstrained(&init_ty) {
                            Type::Error // recovery: nothing usable to bind
                        } else {
                            init_ty // best-effort recovery
                        }
                    }
                };
                self.bind(name, ty, *mutable);
            }
            Stmt::Return { value, span } => {
                let ret = self.ret.clone();
                if let Some(e) = value
                    && self.check_literal_against(e, &ret)
                {
                    return;
                }
                let ty = match value {
                    Some(e) => self.type_of_rhs(e),
                    None => Type::Unit,
                };
                if !fits(&ty, &self.ret) {
                    self.error(
                        format!(
                            "expected return type {}, found {}",
                            self.type_name(&self.ret),
                            self.type_name(&ty)
                        ),
                        *span,
                    );
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.check_condition("if", cond);
                let (if_true, if_false) = condition_facts(cond);
                let then_diverges = diverges(then_body);
                let saved = self.checkpoint(then_diverges);
                let then_survivors = self.check_block_narrowed(then_body, if_true);
                self.rollback(saved);
                match else_body {
                    Some(else_body) => {
                        let else_diverges = diverges(else_body);
                        let saved = self.checkpoint(else_diverges);
                        let else_survivors = self.check_block_narrowed(else_body, if_false);
                        self.rollback(saved);
                        // The join keeps exactly what the unique
                        // fall-through path proves (ADR 0020).
                        match (then_diverges, else_diverges) {
                            (true, false) => self.add_facts(else_survivors),
                            (false, true) => self.add_facts(then_survivors),
                            _ => {}
                        }
                    }
                    // The guard idiom: falling through the if means the
                    // condition was false, and no else existed to
                    // invalidate its facts.
                    None if then_diverges => self.add_facts(if_false),
                    None => {}
                }
            }
            Stmt::While { cond, body, .. } => {
                self.check_condition("while", cond);
                let (if_true, _) = condition_facts(cond);
                self.drop_loop_invalidated_facts(body);
                self.loop_depth += 1;
                self.check_block_narrowed(body, if_true);
                self.loop_depth -= 1;
            }
            Stmt::For {
                index,
                name,
                iterable,
                body,
                span,
            } => {
                if index.as_deref() == Some(name.as_str()) {
                    self.error(
                        format!("index and element need distinct names, both are '{name}'"),
                        *span,
                    );
                }
                let iter_ty = self.type_of_expr(iterable);
                let elem = match iter_ty {
                    // An unconstrained element (`for x in [[]]`) would bind
                    // x at a type that fits everything — reject like an
                    // un-annotated `[]` binding.
                    Type::Array(elem) if unconstrained(&elem) => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                format!("cannot infer a type for '{name}' from this iterable"),
                                iterable.span(),
                            )
                            .with_help("bind the array with an annotated type first".to_string()),
                        );
                        Type::Error
                    }
                    Type::Array(elem) => *elem,
                    ref t if poisoned(t) => Type::Error,
                    other => {
                        self.error(
                            format!(
                                "can only iterate over arrays, found {}",
                                self.type_name(&other)
                            ),
                            iterable.span(),
                        );
                        Type::Error
                    }
                };
                self.drop_loop_invalidated_facts(body);
                // Body scope with the loop bindings: const element (and
                // optional const int index).
                self.nonnull.push(NarrowFrame::new(HashMap::new()));
                self.scopes.push(HashMap::new());
                self.bind(name, elem, false);
                if let Some(index) = index {
                    self.bind(index, Type::Int, false);
                }
                self.loop_depth += 1;
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.loop_depth -= 1;
                self.scopes.pop();
                self.nonnull.pop();
            }
            Stmt::Match {
                scrutinee,
                arms,
                else_body,
                span,
            } => self.check_match(scrutinee, arms, else_body.as_deref(), *span),
            Stmt::Expr(e) => {
                self.type_of_rhs(e);
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    let kw = if matches!(stmt, Stmt::Break { .. }) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.error(format!("'{kw}' outside of a loop"), *span);
                }
            }
            Stmt::Assign {
                target,
                value,
                span,
            } => {
                // Array-literal values are checked against the target's
                // declared type once it's known (ADR 0010); everything else
                // is typed now, while narrowing facts are still intact.
                let value_ty = if matches!(value, Expr::ArrayLit { .. }) {
                    None
                } else {
                    Some(self.type_of_rhs(value))
                };
                // The parser only builds place targets, so a root always exists.
                let Some((root, root_span)) = root_ident(target) else {
                    return;
                };
                let Some(mutable) = self.find_var(root).map(|info| info.mutable) else {
                    self.lookup(root, root_span); // emits undefined + suggestion
                    return;
                };
                // Rebinding a place invalidates its narrowing — the new
                // value may be null again. (The value above was typed while
                // still narrowed, so `cur = cur.next` checks out. Prefixes
                // stay narrowed, so a guarded `cur.left.v = 1` still types.)
                if self.has_facts()
                    && let Some(path) = target.place_path()
                {
                    self.unnarrow(&path);
                }
                // Typing the target may emit its own errors (unknown field);
                // stop here when it does — mutability/mismatch checks on an
                // ill-formed target would only add noise.
                let before = self.diagnostics.len();
                let target_ty = self.type_of_expr(target);
                let clean = self.diagnostics.len() == before;
                // Mutability: a `var` root, or a chain crossing a refstruct
                // boundary (past a reference we mutate the shared object,
                // not the binding). Decided while narrowing facts are still
                // intact, so crosses_ref's re-typing sees exactly what the
                // typing pass above saw and emits nothing new.
                let allowed = !clean || mutable || self.crosses_ref(target);
                // A write through a field or index may reach aliased state —
                // field facts don't survive it. Dropped only now, after
                // every check that re-reads the place.
                if !matches!(target, Expr::Ident(..)) {
                    self.unnarrow_field_paths();
                }
                if !clean {
                    return;
                }
                if !allowed {
                    self.error(format!("cannot assign to const '{root}'"), *span);
                    return;
                }
                let value_ty = match value_ty {
                    Some(ty) => ty,
                    None if self.check_literal_against(value, &target_ty) => return,
                    None => self.type_of_expr(value),
                };
                if !fits(&value_ty, &target_ty) {
                    let message = match target {
                        Expr::Field { name, .. } => format!(
                            "field '{name}' expects {}, found {}",
                            self.type_name(&target_ty),
                            self.type_name(&value_ty)
                        ),
                        _ => format!(
                            "cannot assign {} to variable of type {}",
                            self.type_name(&value_ty),
                            self.type_name(&target_ty)
                        ),
                    };
                    self.error(message, *span);
                }
            }
        }
    }
}
