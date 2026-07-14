//! Statement checking and flow-sensitive narrowing: function bodies,
//! bindings, assignment legality (the var-root / refstruct-crossing
//! rule), and the null-narrowing fact stack — facts appear on
//! `!= null` guards and die on any write, call, or rebind that could
//! invalidate them (soundness over convenience, ADR 0007).

use super::*;

impl Checker<'_> {
    pub(super) fn check_function(&mut self, f: &Function) {
        let sig = self.sigs[&(self.module, f.name.clone())].clone();
        self.ret = sig.ret;
        let mut scope = HashMap::new();
        for (param, ty) in f.params.iter().zip(sig.params) {
            scope.insert(param.name.clone(), VarInfo { ty, mutable: false });
        }
        self.scopes.push(scope);
        for stmt in &f.body {
            self.check_stmt(stmt);
        }
        self.scopes.pop();

        if self.ret != Type::Unit && !poisoned(&self.ret) && !always_returns(&f.body) {
            self.error(
                format!("not all paths in function '{}' return a value", f.name),
                f.span,
            );
        }
    }

    /// Type-checks a nested block in its own scope (bindings made inside die
    /// at the closing brace), with a set of place paths proven non-null for
    /// its duration.
    fn check_block_narrowed(&mut self, stmts: &[Stmt], facts: HashSet<String>) {
        self.nonnull.push(NarrowFrame::new(facts));
        self.scopes.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        self.scopes.pop();
        self.nonnull.pop();
    }

    /// Any narrowing facts at all? The cheap gate for the common,
    /// un-narrowed path — skips fact bookkeeping entirely.
    pub(super) fn has_facts(&self) -> bool {
        self.nonnull.iter().any(|f| !f.facts.is_empty())
    }

    pub(super) fn is_nonnull(&self, path: &str) -> bool {
        // Innermost first: a shadow hides outer facts for its own region,
        // but a re-established inner fact wins over an outer shadow.
        for frame in self.nonnull.iter().rev() {
            if frame.shadowed.iter().any(|s| covers(s, path)) {
                return false;
            }
            if frame.facts.contains(path) {
                return true;
            }
        }
        false
    }

    /// Permanently drops a place path — and everything reached through it —
    /// from every narrowing frame. Used when the place is reassigned.
    pub(super) fn unnarrow(&mut self, path: &str) {
        for frame in &mut self.nonnull {
            frame.facts.retain(|q| !covers(path, q));
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
            frame.facts.retain(|q| !q.contains('.'));
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
                        let declared = resolve_type(ann, self.ty_alias, *span, self.diagnostics);
                        // Codegen gates bindings on this resolved type —
                        // never on the raw annotation.
                        self.let_types.insert(*span, declared.clone());
                        if !self.check_literal_against(value, &declared) {
                            let init_ty = self.type_of_expr(value);
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
                        let init_ty = self.type_of_expr(value);
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
                if let Some(e) = value {
                    if self.check_literal_against(e, &ret) {
                        return;
                    }
                }
                let ty = match value {
                    Some(e) => self.type_of_expr(e),
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
                let (if_true, if_false) = null_checks(cond);
                self.check_block_narrowed(then_body, if_true);
                if let Some(else_body) = else_body {
                    self.check_block_narrowed(else_body, if_false);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.check_condition("while", cond);
                let (if_true, _) = null_checks(cond);
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
                self.nonnull.push(NarrowFrame::new(HashSet::new()));
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
            Stmt::Expr(e) => {
                self.type_of_expr(e);
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
                    Some(self.type_of_expr(value))
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
                if self.has_facts() {
                    if let Some(path) = target.place_path() {
                        self.unnarrow(&path);
                    }
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
