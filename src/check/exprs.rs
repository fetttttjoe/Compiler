//! Expression typing. `type_of_expr` is the single choke point — every
//! typed expression is recorded in the per-expression type table that
//! the backend reads (it never re-derives a type). Calls, field access,
//! struct literals, and declaration-checked array literals each get
//! their own rule below.

use super::*;

impl Checker<'_> {
    // Recursion here (and in every later pass) is stack-safe because the
    // parser bounds AST height at construction (`MAX_FN_OPS`), and the
    // pipeline runs on a worker stack sized for that bound (main.rs).
    pub(super) fn type_of_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.type_of_expr_inner(expr);
        // The per-expression type table (Resolutions::expr_types) — the
        // one choke point every typing pass flows through.
        self.expr_types.insert(expr.span(), ty.clone());
        ty
    }

    fn type_of_expr_inner(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Int(_, _) => Type::Int,
            Expr::Float(_, _) => Type::Float,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Str(_, _) => Type::Str,
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Null(_) => Type::Null,
            // Conversions cross-convert only (ADR 0028): identity
            // conversions are rejected — a no-op spelled as a conversion
            // is noise, not explicitness. `string(x)` (ADR 0029) renders
            // any value as `print` would; only the identity and the
            // no-value types (`unit`, `null`) are rejected. Narrowing
            // applies — a narrowed `string?` is a `string`. A template's
            // `${e}` (ADR 0030) is the implicit form: there the string
            // identity passes through — the template's `+` does the work.
            Expr::Convert {
                to,
                implicit,
                arg,
                span,
            } => {
                let ty = self.type_of_expr(arg);
                if poisoned(&ty) {
                    return Type::Error;
                }
                if *to == Conv::Str {
                    if matches!(ty, Type::Str | Type::Unit | Type::Null) {
                        if *implicit && ty == Type::Str {
                            return Type::Str;
                        }
                        let mut diag = Diagnostic::error(
                            if *implicit {
                                format!("cannot interpolate {} in a template", self.type_name(&ty))
                            } else {
                                format!("string() cannot convert {}", self.type_name(&ty))
                            },
                            *span,
                        );
                        if !*implicit && ty == Type::Str {
                            diag = diag.with_help("the value is already string".to_string());
                        }
                        self.diagnostics.push(diag);
                        return Type::Error;
                    }
                    return Type::Str;
                }
                let (want, result) = if *to == Conv::Float {
                    (Type::Int, Type::Float)
                } else {
                    (Type::Float, Type::Int)
                };
                if ty != want {
                    let mut diag = Diagnostic::error(
                        format!(
                            "{}() expects {}, found {}",
                            to.keyword(),
                            self.type_name(&want),
                            self.type_name(&ty)
                        ),
                        *span,
                    );
                    if ty == result {
                        diag =
                            diag.with_help(format!("the value is already {}", self.type_name(&ty)));
                    }
                    self.diagnostics.push(diag);
                    return Type::Error;
                }
                result
            }
            Expr::Unary { op, rhs, span } => {
                let ty = self.type_of_expr(rhs);
                match op {
                    _ if poisoned(&ty) => Type::Error,
                    UnOp::Neg if is_numeric(&ty) => ty,
                    UnOp::Neg => {
                        self.error(format!("cannot negate {}", self.type_name(&ty)), *span);
                        Type::Error
                    }
                    UnOp::Not if ty == Type::Bool => Type::Bool,
                    UnOp::Not => {
                        self.error(
                            format!("cannot apply '!' to {}", self.type_name(&ty)),
                            *span,
                        );
                        Type::Error
                    }
                }
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let lt = self.type_of_expr(lhs);
                // `x != null && …` — the null check guards the right side.
                let rt = if *op == BinOp::And {
                    let (if_true, _) = null_checks(lhs);
                    self.nonnull.push(NarrowFrame::new(if_true));
                    let rt = self.type_of_expr(rhs);
                    self.nonnull.pop();
                    rt
                } else {
                    self.type_of_expr(rhs)
                };
                self.check_binary(*op, lt, rt, *span)
            }
            Expr::Call { callee, args, span } => {
                let ty = self.check_call(callee, args, *span);
                // The callee can mutate any shared refstruct it can reach,
                // so field-path narrowing doesn't survive a call.
                self.unnarrow_field_paths();
                ty
            }
            Expr::Field {
                base,
                name,
                optional,
                span,
            } => self.check_field(base, name, *optional, *span),
            Expr::StructLit { name, fields, span } => self.check_struct_lit(name, fields, *span),
            Expr::ArrayLit { elements, .. } => {
                // The first element names the type; a later `null` widens
                // it to optional; `[]` is unconstrained and fits any array
                // slot (but a binding then needs an annotation).
                // ponytail: first-element inference — `[null, x]` needs
                // reordering; real unification when evidence demands it.
                let mut rest = elements.iter();
                let mut elem_ty = match rest.next().map(|e| (e, self.type_of_expr(e))) {
                    None => Type::Unknown,
                    Some((e, Type::Null)) => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "array element type cannot be inferred from 'null'".to_string(),
                                e.span(),
                            )
                            .with_help("start with a non-null element, e.g. [x, null]".to_string()),
                        );
                        Type::Error
                    }
                    Some((_, ty)) => ty,
                };
                for element in rest {
                    let ty = self.type_of_expr(element);
                    if ty == Type::Null && !matches!(elem_ty, Type::Optional(_)) {
                        elem_ty = Type::Optional(Box::new(elem_ty));
                        continue;
                    }
                    // A later element can pin down what a leading `[]`
                    // left open: `[[], [1]]` is int[][].
                    if unconstrained(&elem_ty) && !unconstrained(&ty) {
                        elem_ty = ty;
                        continue;
                    }
                    if !fits(&ty, &elem_ty) {
                        self.error(
                            format!(
                                "array elements must share one type: expected {}, found {}",
                                self.type_name(&elem_ty),
                                self.type_name(&ty)
                            ),
                            element.span(),
                        );
                    }
                }
                Type::Array(Box::new(elem_ty))
            }
            Expr::Index { base, index, span } => {
                let base_ty = self.type_of_expr(base);
                let index_ty = self.type_of_expr(index);
                if !fits(&index_ty, &Type::Int) {
                    self.error(
                        format!("index must be int, found {}", self.type_name(&index_ty)),
                        index.span(),
                    );
                }
                match base_ty {
                    Type::Array(elem) => *elem,
                    ref t if poisoned(t) => Type::Error,
                    Type::Optional(_) => {
                        self.diagnostics.push(
                            Diagnostic::error(
                                format!(
                                    "{} may be null — it can't be indexed directly",
                                    self.type_name(&base_ty)
                                ),
                                *span,
                            )
                            .with_help("check '!= null' first".to_string()),
                        );
                        Type::Error
                    }
                    other => {
                        self.error(
                            format!("cannot index into {}", self.type_name(&other)),
                            *span,
                        );
                        Type::Error
                    }
                }
            }
        }
    }

    fn check_binary(&mut self, op: BinOp, lt: Type, rt: Type, span: Span) -> Type {
        use BinOp::*;
        // A poisoned operand already produced its diagnostic — stay silent.
        if poisoned(&lt) || poisoned(&rt) {
            return Type::Error;
        }
        let (ok, result) = match op {
            // Arithmetic on matching numerics; `+` also concatenates strings.
            Add | Sub | Mul | Div | Rem => {
                let ok = lt == rt && (is_numeric(&lt) || (op == Add && lt == Type::Str));
                (ok, lt.clone())
            }
            // Ordering on matching numerics.
            Lt | Le | Gt | Ge => (lt == rt && is_numeric(&lt), Type::Bool),
            // Equality on any matching primitive or struct type (struct
            // identity is (module, name)), plus null checks on optionals.
            Eq | Ne => (eq_comparable(&lt, &rt), Type::Bool),
            // Logic on bools.
            And | Or => (lt == Type::Bool && rt == Type::Bool, Type::Bool),
            // `a ?? b`: a must be optional; b re-fills it (`T` unwraps,
            // `T?`/null keep it optional).
            Coalesce => match &lt {
                Type::Optional(inner) => {
                    let result = if rt == **inner {
                        (**inner).clone()
                    } else {
                        lt.clone()
                    };
                    (fits(&rt, &lt), result)
                }
                _ => (false, lt.clone()),
            },
        };
        if !ok {
            self.error(
                format!(
                    "cannot apply '{}' to {} and {}",
                    op.symbol(),
                    self.type_name(&lt),
                    self.type_name(&rt)
                ),
                span,
            );
            return Type::Error;
        }
        result
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Type {
        let name = match callee {
            Expr::Ident(n, _) => n.clone(),
            _ => {
                self.error("only named functions can be called".to_string(), span);
                return Type::Error;
            }
        };
        // Copy the map references out of `self` so the signature borrow is
        // independent of the `&mut self` calls below — no clone needed.
        let sigs = self.sigs;
        let Some(target) = self.fn_alias.get(&name) else {
            // Builtins resolve only when no user definition shadows them.
            if name == syntax::BUILTIN_PRINT {
                if args.len() != 1 {
                    self.error(
                        format!("'print' expects 1 argument, found {}", args.len()),
                        span,
                    );
                }
                for arg in args {
                    self.type_of_expr(arg); // any type prints
                }
                return Type::Unit;
            }
            if name == syntax::BUILTIN_LEN {
                if args.len() != 1 {
                    self.error(
                        format!("'len' expects 1 argument, found {}", args.len()),
                        span,
                    );
                }
                for arg in args {
                    let ty = self.type_of_expr(arg);
                    if !matches!(ty, Type::Array(_)) && !poisoned(&ty) {
                        self.error(
                            format!("'len' expects an array, found {}", self.type_name(&ty)),
                            arg.span(),
                        );
                    }
                }
                return Type::Int;
            }
            // The world interface (ADR 0031). Failure is a value:
            // open/read/readLine produce optionals, write/close bools.
            if name == syntax::BUILTIN_OPEN {
                self.expect_builtin_args(&name, args, &[Type::Str, Type::Str], span);
                return Type::Optional(Box::new(Type::File));
            }
            if name == syntax::BUILTIN_READ {
                self.expect_builtin_args(&name, args, &[Type::File, Type::Int], span);
                return Type::Optional(Box::new(Type::Str));
            }
            if name == syntax::BUILTIN_READLINE {
                // Arity 0 reads stdin; arity 1 reads a file.
                if !args.is_empty() {
                    self.expect_builtin_args(&name, args, &[Type::File], span);
                }
                return Type::Optional(Box::new(Type::Str));
            }
            if name == syntax::BUILTIN_WRITE {
                self.expect_builtin_args(&name, args, &[Type::File, Type::Str], span);
                return Type::Bool;
            }
            if name == syntax::BUILTIN_CLOSE {
                self.expect_builtin_args(&name, args, &[Type::File], span);
                return Type::Bool;
            }
            if name == syntax::BUILTIN_PUSH {
                if args.len() != 2 {
                    self.error(
                        format!("'push' expects 2 arguments, found {}", args.len()),
                        span,
                    );
                    for arg in args {
                        self.type_of_expr(arg);
                    }
                    return Type::Unit;
                }
                let array_ty = self.type_of_expr(&args[0]);
                let value_ty = self.type_of_expr(&args[1]);
                match array_ty {
                    Type::Array(elem) => {
                        if !fits(&value_ty, &elem) {
                            self.error(
                                format!(
                                    "'push' into {}[]: expected {}, found {}",
                                    self.type_name(&elem),
                                    self.type_name(&elem),
                                    self.type_name(&value_ty)
                                ),
                                args[1].span(),
                            );
                        }
                    }
                    ref t if poisoned(t) => {}
                    other => self.error(
                        format!("'push' expects an array, found {}", self.type_name(&other)),
                        args[0].span(),
                    ),
                }
                return Type::Unit;
            }
            self.diagnostics.push(
                Diagnostic::error(format!("undefined function '{name}'"), span)
                    .suggest(&name, self.fn_alias.keys().map(String::as_str)),
            );
            // Still check the arguments — their own errors shouldn't vanish
            // just because the callee is unknown.
            for arg in args {
                self.type_of_expr(arg);
            }
            return Type::Error;
        };
        let sig = &sigs[target];
        if args.len() != sig.params.len() {
            self.error(
                format!(
                    "function '{}' expects {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
                span,
            );
        }
        for (arg, expected) in args.iter().zip(&sig.params) {
            if self.check_literal_against(arg, expected) {
                continue;
            }
            let got = self.type_of_expr(arg);
            if !fits(&got, expected) {
                self.error(
                    format!(
                        "expected argument of type {}, found {}",
                        self.type_name(expected),
                        self.type_name(&got)
                    ),
                    arg.span(),
                );
            }
        }
        sig.ret.clone()
    }

    /// Bidirectional check for array literals at declared positions: the
    /// declaration is the truth, never the literal's shape (ADR 0010).
    /// Unwraps optional declarations, recurses into nested literals, and
    /// returns true when it handled the pair — callers fall back to the
    /// ordinary `fits` path otherwise.
    pub(super) fn check_literal_against(&mut self, value: &Expr, expected: &Type) -> bool {
        let mut target = expected;
        while let Type::Optional(inner) = target {
            target = inner;
        }
        let (Type::Array(elem), Expr::ArrayLit { elements, .. }) = (target, value) else {
            return false;
        };
        // This path bypasses type_of_expr, but the per-expression type
        // table must stay total: the literal's type IS the declared one.
        self.expr_types.insert(value.span(), expected.clone());
        for element in elements {
            if self.check_literal_against(element, elem) {
                continue;
            }
            let got = self.type_of_expr(element);
            if !fits(&got, elem) {
                let mut diag = Diagnostic::error(
                    format!(
                        "array element: expected {}, found {}",
                        self.type_name(elem),
                        self.type_name(&got)
                    ),
                    element.span(),
                );
                if got == Type::Null {
                    diag = diag.with_help(format!(
                        "declare the element type optional: '{}?'",
                        self.type_name(elem)
                    ));
                }
                self.diagnostics.push(diag);
            }
        }
        true
    }

    fn check_field(&mut self, base: &Expr, field: &str, optional: bool, span: Span) -> Type {
        let base_ty = self.type_of_expr(base);
        // Stage 1: the optional layer — `?.` must strip one, `.` must not
        // have one to strip.
        let inner = match (&base_ty, optional) {
            // A poisoned base already produced its diagnostic.
            (t, _) if poisoned(t) => return Type::Error,
            (Type::Optional(inner), true) => inner.as_ref(),
            (Type::Optional(_), false) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!(
                            "{} may be null — its fields can't be read directly",
                            self.type_name(&base_ty)
                        ),
                        span,
                    )
                    .with_help("use '?.', or check '!= null' first".to_string()),
                );
                return Type::Error;
            }
            (_, true) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        format!("'?.' on {}, which is never null", self.type_name(&base_ty)),
                        span,
                    )
                    .with_help("use '.'".to_string()),
                );
                return Type::Error;
            }
            (ty, false) => ty,
        };
        // Stage 2: the field lookup, shared by both forms.
        let Type::Struct(sm, struct_name) = inner else {
            self.error(
                format!("type {} has no fields", self.type_name(&base_ty)),
                span,
            );
            return Type::Error;
        };
        let field_ty = self
            .structs
            .get(&(*sm, struct_name.clone()))
            .and_then(|st| {
                st.fields
                    .iter()
                    .position(|(fname, _)| fname == field)
                    .map(|i| (i, st.fields[i].1.clone()))
            })
            .map(|(i, ty)| {
                // Codegen's layout table (field offsets are computed from
                // the base def, since fields may be multi-word).
                self.field_slots.insert(
                    span,
                    FieldSlot {
                        base: (*sm, struct_name.clone()),
                        index: i,
                        ty: ty.clone(),
                    },
                );
                ty
            });
        match field_ty {
            Some(ty) if optional => match ty {
                // `a?.b` is optional; an already-optional field stays flat.
                Type::Optional(_) => ty,
                other => Type::Optional(Box::new(other)),
            },
            // A narrowed field path reads as its inner type, same as
            // narrowed variables in `lookup`.
            Some(Type::Optional(inner))
                if self.has_facts()
                    && base
                        .place_path()
                        .is_some_and(|p| self.is_nonnull(&format!("{p}.{field}"))) =>
            {
                *inner
            }
            Some(ty) => ty,
            None => {
                self.error(
                    format!("struct '{struct_name}' has no field '{field}'"),
                    span,
                );
                Type::Error
            }
        }
    }

    fn check_struct_lit(&mut self, name: &str, fields: &[(String, Expr)], span: Span) -> Type {
        let structs = self.structs;
        let Some(key) = self.ty_alias.get(name) else {
            self.diagnostics.push(
                Diagnostic::error(format!("unknown struct '{name}'"), span)
                    .suggest(name, self.ty_alias.keys().map(String::as_str)),
            );
            for (_, value) in fields {
                self.type_of_expr(value);
            }
            return Type::Error;
        };
        let decl = &structs[key];
        let mut seen = HashSet::new();
        for (fname, value) in fields {
            if !seen.insert(fname.clone()) {
                self.error(
                    format!("duplicate field '{fname}' in struct literal"),
                    value.span(),
                );
            }
            if let Some((_, expected)) = decl.fields.iter().find(|(dn, _)| dn == fname)
                && self.check_literal_against(value, &expected.clone())
            {
                continue;
            }
            let got = self.type_of_expr(value);
            match decl.fields.iter().find(|(dn, _)| dn == fname) {
                Some((_, expected)) => {
                    if !fits(&got, expected) {
                        self.error(
                            format!(
                                "field '{fname}' expects {}, found {}",
                                self.type_name(expected),
                                self.type_name(&got)
                            ),
                            value.span(),
                        );
                    }
                }
                None => self.error(
                    format!("struct '{name}' has no field '{fname}'"),
                    value.span(),
                ),
            }
        }
        for (dn, _) in &decl.fields {
            if !fields.iter().any(|(fname, _)| fname == dn) {
                self.error(format!("missing field '{dn}' in struct '{name}'"), span);
            }
        }
        Type::Struct(key.0, key.1.clone())
    }

    /// True when mutating through this place chain passes a refstruct
    /// boundary (some field access has a by-ref base). Bases get re-typed,
    /// but only after the whole target typed cleanly, so no duplicate
    /// diagnostics are emitted.
    pub(super) fn crosses_ref(&mut self, place: &Expr) -> bool {
        match place {
            Expr::Field { base, .. } => {
                let base_ty = self.type_of_expr(base);
                self.is_by_ref(&base_ty) || self.crosses_ref(base)
            }
            // Arrays are references — element writes never touch the binding.
            Expr::Index { .. } => true,
            _ => false,
        }
    }

    fn is_by_ref(&self, t: &Type) -> bool {
        match t {
            Type::Struct(m, n) => self.structs.get(&(*m, n.clone())).is_some_and(|s| s.by_ref),
            _ => false,
        }
    }

    /// Declares `name` in the innermost scope and hides any narrowing of
    /// the same name while that scope's region lives — a new binding
    /// shadows; outer facts return when the frame pops. Same-frame facts
    /// about the name die here: they were about the previous binding, and
    /// facts outrank the shadow within a frame (ADR 0033).
    pub(super) fn bind(&mut self, name: &str, ty: Type, mutable: bool) {
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), VarInfo { ty, mutable });
        if let Some(frame) = self.nonnull.last_mut() {
            frame.facts.retain(|f| !covers(name, f));
            frame.shadowed.insert(name.to_string());
        }
    }

    /// The innermost binding for `name`, searching scopes inside-out.
    pub(super) fn find_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Arity and per-argument typing for a fixed-signature builtin;
    /// arguments are always typed (even on arity errors) for recovery.
    fn expect_builtin_args(&mut self, name: &str, args: &[Expr], want: &[Type], span: Span) {
        if args.len() != want.len() {
            self.error(
                format!(
                    "'{name}' expects {} argument{}, found {}",
                    want.len(),
                    if want.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                span,
            );
            for arg in args {
                self.type_of_expr(arg);
            }
            return;
        }
        for (arg, want) in args.iter().zip(want) {
            let ty = self.type_of_expr(arg);
            if !fits(&ty, want) && !poisoned(&ty) {
                self.error(
                    format!(
                        "'{name}' expects {}, found {}",
                        self.type_name(want),
                        self.type_name(&ty)
                    ),
                    arg.span(),
                );
            }
        }
    }

    pub(super) fn lookup(&mut self, name: &str, span: Span) -> Type {
        if let Some(info) = self.find_var(name) {
            // A narrowed optional reads as its inner type.
            if self.is_nonnull(name)
                && let Type::Optional(inner) = &info.ty
            {
                return (**inner).clone();
            }
            return info.ty.clone();
        }
        let visible = self
            .scopes
            .iter()
            .flat_map(|s| s.keys().map(String::as_str));
        self.diagnostics.push(
            Diagnostic::error(format!("undefined variable '{name}'"), span).suggest(name, visible),
        );
        Type::Error
    }
}
