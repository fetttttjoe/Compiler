use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::narrow::{body_effects, covers, null_checks, NarrowFrame};
use crate::span::Span;
use crate::syntax;
use crate::types::{
    eq_comparable, fits, is_numeric, poisoned, unconstrained, FnSig, StructType, Type,
};

/// A per-module view: visible name → the (module, name) that defines it.
type Alias = HashMap<String, (usize, String)>;

/// Name-resolution results — the checker's durable output. For every module,
/// each callable name maps to its defining (module, name), locals and imports
/// alike. Consumed by the interpreter (and later codegen).
pub struct Resolutions {
    pub functions: Vec<Alias>,
    /// Per module: the visible type names that denote `refstruct`s, so the
    /// interpreter (and later codegen) knows which literals allocate a
    /// shared object instead of a value.
    pub ref_structs: Vec<HashSet<String>>,
}

/// One module's declared names with their export flags.
struct ModuleNames {
    fns: HashMap<String, bool>,
    structs: HashMap<String, bool>,
}

/// Static type checking over the whole module graph. Empty diagnostics =
/// well-typed program.
pub fn check(graph: &ModuleGraph) -> (Resolutions, Vec<Diagnostic>) {
    let mut diags = Vec::new();

    // Pass A: every module's own names + intra-file duplicate detection.
    let names: Vec<ModuleNames> = graph
        .modules
        .iter()
        .map(|m| collect_names(&m.ast, &mut diags))
        .collect();

    // Pass B: alias maps — locals plus imported bindings, with import errors
    // (unknown item, not exported, collision within this file).
    let mut fn_aliases: Vec<Alias> = Vec::new();
    let mut ty_aliases: Vec<Alias> = Vec::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        let mut fn_alias: Alias = names[mi]
            .fns
            .keys()
            .map(|n| (n.clone(), (mi, n.clone())))
            .collect();
        let mut ty_alias: Alias = names[mi]
            .structs
            .keys()
            .map(|n| (n.clone(), (mi, n.clone())))
            .collect();
        for binding in &module.imports {
            let target = &names[binding.target];
            let target_path = &graph.modules[binding.target].path;
            let fn_export = target.fns.get(&binding.name).copied();
            let ty_export = target.structs.get(&binding.name).copied();
            if fn_export.is_none() && ty_export.is_none() {
                let exported_names = target
                    .fns
                    .iter()
                    .chain(target.structs.iter())
                    .filter(|(_, &exported)| exported)
                    .map(|(n, _)| n.as_str());
                diags.push(
                    Diagnostic::error(
                        format!("module '{target_path}' has no item '{}'", binding.name),
                        binding.span,
                    )
                    .suggest(&binding.name, exported_names),
                );
                continue;
            }
            if fn_export != Some(true) && ty_export != Some(true) {
                diags.push(
                    Diagnostic::error(
                        format!(
                            "'{}' exists in '{target_path}' but is not exported",
                            binding.name
                        ),
                        binding.span,
                    )
                    .with_help(format!(
                        "add 'export' before the definition of '{}' in '{target_path}'",
                        binding.name
                    )),
                );
                continue;
            }
            if fn_alias.contains_key(&binding.name) || ty_alias.contains_key(&binding.name) {
                diags.push(Diagnostic::error(
                    format!("'{}' is already defined in this file", binding.name),
                    binding.span,
                ));
                continue;
            }
            if fn_export == Some(true) {
                fn_alias.insert(binding.name.clone(), (binding.target, binding.name.clone()));
            }
            if ty_export == Some(true) {
                ty_alias.insert(binding.name.clone(), (binding.target, binding.name.clone()));
            }
        }
        fn_aliases.push(fn_alias);
        ty_aliases.push(ty_alias);
    }

    // Pass C: resolve signatures and struct layouts through the alias maps.
    let mut sigs: HashMap<(usize, String), FnSig> = HashMap::new();
    let mut structs: HashMap<(usize, String), StructType> = HashMap::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            match item {
                Item::Struct(s) => {
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                resolve_type(&f.ty, &ty_aliases[mi], s.span, &mut diags),
                            )
                        })
                        .collect();
                    structs.insert(
                        (mi, s.name.clone()),
                        StructType {
                            fields,
                            by_ref: s.by_ref,
                        },
                    );
                }
                Item::Function(f) => {
                    let params = f
                        .params
                        .iter()
                        .map(|p| resolve_type(&p.ty, &ty_aliases[mi], f.span, &mut diags))
                        .collect();
                    let ret = match &f.return_type {
                        Some(t) => resolve_type(t, &ty_aliases[mi], f.span, &mut diags),
                        None => Type::Unit,
                    };
                    sigs.insert((mi, f.name.clone()), FnSig { params, ret });
                }
                Item::Import(_) => {}
            }
        }
    }

    // Pass D: check every function body against its module's view.
    let paths: Vec<&str> = graph.modules.iter().map(|m| m.path.as_str()).collect();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item {
                let mut checker = Checker {
                    module: mi,
                    paths: &paths,
                    fn_alias: &fn_aliases[mi],
                    ty_alias: &ty_aliases[mi],
                    sigs: &sigs,
                    structs: &structs,
                    diagnostics: &mut diags,
                    scopes: Vec::new(),
                    nonnull: Vec::new(),
                    ret: Type::Unit,
                };
                checker.check_function(f);
            }
        }
    }

    let ref_structs = ty_aliases
        .iter()
        .map(|alias| {
            alias
                .iter()
                .filter(|(_, key)| structs.get(*key).is_some_and(|s| s.by_ref))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .collect();

    (
        Resolutions {
            functions: fn_aliases,
            ref_structs,
        },
        diags,
    )
}

fn collect_names(ast: &[Item], diags: &mut Vec<Diagnostic>) -> ModuleNames {
    let mut names = ModuleNames {
        fns: HashMap::new(),
        structs: HashMap::new(),
    };
    for item in ast {
        match item {
            Item::Function(f) => {
                if names.fns.insert(f.name.clone(), f.exported).is_some() {
                    diags.push(Diagnostic::error(
                        format!("function '{}' is already defined", f.name),
                        f.span,
                    ));
                }
            }
            Item::Struct(s) => {
                if names.structs.insert(s.name.clone(), s.exported).is_some() {
                    diags.push(Diagnostic::error(
                        format!("struct '{}' is already defined", s.name),
                        s.span,
                    ));
                }
            }
            Item::Import(_) => {}
        }
    }
    names
}

fn resolve_type(ann: &TypeAnn, ty_alias: &Alias, span: Span, diags: &mut Vec<Diagnostic>) -> Type {
    match ann {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Bool => Type::Bool,
        TypeAnn::Str => Type::Str,
        TypeAnn::Optional(inner) => {
            Type::Optional(Box::new(resolve_type(inner, ty_alias, span, diags)))
        }
        TypeAnn::Array(inner) => Type::Array(Box::new(resolve_type(inner, ty_alias, span, diags))),
        TypeAnn::Named(name) => match ty_alias.get(name) {
            Some((m, n)) => Type::Struct(*m, n.clone()),
            None => {
                diags.push(
                    Diagnostic::error(format!("unknown type '{name}'"), span)
                        .suggest(name, ty_alias.keys().map(String::as_str)),
                );
                Type::Error // recovery: already reported
            }
        },
    }
}

struct VarInfo {
    ty: Type,
    mutable: bool,
}

struct Checker<'a> {
    module: usize,
    paths: &'a [&'a str],
    fn_alias: &'a Alias,
    ty_alias: &'a Alias,
    sigs: &'a HashMap<(usize, String), FnSig>,
    structs: &'a HashMap<(usize, String), StructType>,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<HashMap<String, VarInfo>>,
    /// Narrowing stack: place paths (`cur`, `cur.left`) proven non-null by
    /// an enclosing `!= null` check. A narrowed `T?` reads as `T`. Rebinding
    /// removes a path and its extensions permanently; shadowing only hides
    /// it while the shadow's frame lives; field paths are also dropped on
    /// any call or field write, since aliases can reach them.
    nonnull: Vec<NarrowFrame>,
    ret: Type,
}

impl<'a> Checker<'a> {
    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }

    /// A type name for messages: structs from *other* modules carry their
    /// defining file, so same-named types stay distinguishable
    /// (`P (from a.ys)` vs `P`).
    fn type_name(&self, t: &Type) -> String {
        match t {
            Type::Struct(m, n) if *m != self.module => {
                format!("{n} (from {})", self.paths[*m])
            }
            Type::Optional(inner) => format!("{}?", self.type_name(inner)),
            Type::Array(inner) if unconstrained(inner) => "[]".to_string(),
            Type::Array(inner) => format!("{}[]", self.type_name(inner)),
            _ => t.name(),
        }
    }

    fn check_function(&mut self, f: &Function) {
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
    fn has_facts(&self) -> bool {
        self.nonnull.iter().any(|f| !f.facts.is_empty())
    }

    fn is_nonnull(&self, path: &str) -> bool {
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
    fn unnarrow(&mut self, path: &str) {
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
    fn unnarrow_field_paths(&mut self) {
        for frame in &mut self.nonnull {
            frame.facts.retain(|q| !q.contains('.'));
        }
    }

    fn check_condition(&mut self, keyword: &str, cond: &Expr) {
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
                self.check_block_narrowed(body, if_true);
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
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.scopes.pop();
                self.nonnull.pop();
            }
            Stmt::Expr(e) => {
                self.type_of_expr(e);
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

    // Recursion here (and in every later pass) is stack-safe because the
    // parser bounds AST height at construction (`MAX_FN_OPS`), and the
    // pipeline runs on a worker stack sized for that bound (main.rs).
    fn type_of_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Int(_, _) => Type::Int,
            Expr::Float(_, _) => Type::Float,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Str(_, _) => Type::Str,
            Expr::Ident(name, span) => self.lookup(name, *span),
            Expr::Null(_) => Type::Null,
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
    fn check_literal_against(&mut self, value: &Expr, expected: &Type) -> bool {
        let mut target = expected;
        while let Type::Optional(inner) = target {
            target = inner;
        }
        let (Type::Array(elem), Expr::ArrayLit { elements, .. }) = (target, value) else {
            return false;
        };
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
            .and_then(|st| st.fields.iter().find(|(fname, _)| fname == field))
            .map(|(_, ty)| ty.clone());
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
            if let Some((_, expected)) = decl.fields.iter().find(|(dn, _)| dn == fname) {
                if self.check_literal_against(value, &expected.clone()) {
                    continue;
                }
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
    fn crosses_ref(&mut self, place: &Expr) -> bool {
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
    /// shadows; outer facts return when the frame pops.
    fn bind(&mut self, name: &str, ty: Type, mutable: bool) {
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), VarInfo { ty, mutable });
        if let Some(frame) = self.nonnull.last_mut() {
            frame.shadowed.insert(name.to_string());
        }
    }

    /// The innermost binding for `name`, searching scopes inside-out.
    fn find_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn lookup(&mut self, name: &str, span: Span) -> Type {
        if let Some(info) = self.find_var(name) {
            // A narrowed optional reads as its inner type.
            if self.is_nonnull(name) {
                if let Type::Optional(inner) = &info.ty {
                    return (**inner).clone();
                }
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

/// The variable at the root of a place expression (`o.i.v` → `o`).
fn root_ident(e: &Expr) -> Option<(&str, Span)> {
    match e {
        Expr::Ident(n, s) => Some((n, *s)),
        Expr::Field { base, .. } | Expr::Index { base, .. } => root_ident(base),
        _ => None,
    }
}

/// Definite-return analysis: does this statement list guarantee a `return` on
/// every path? An `if` guarantees it only when both branches exist and both
/// return; a `while` never does (its condition can be false on entry).
fn always_returns(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } => true,
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => always_returns(then_body) && always_returns(else_body),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::{load_program, Module};
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
        assert!(d
            .iter()
            .any(|e| e.message.contains("unknown type 'Missing'")));
    }

    fn diags(src: &str) -> Vec<Diagnostic> {
        check(&graph_of(src)).1
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

        let d = diags(
            "struct Point { x: int } fun f(): int { const p: Pont = Pont { x: 1 }; return 1; }",
        );
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
        let d =
            diags("struct P { x: int } fun f(): int { const p: P = P { x: 1, x: 2 }; return 1; }");
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
        let d = diags(
            "struct P { x: int, y: int } fun f(): int { const p: P = P { x: 1 }; return 1; }",
        );
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
        let d =
            diags("refstruct P { x: int } fun f() { const p: P = P { x: 1 }; p = P { x: 2 }; }");
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
    fn early_return_does_not_narrow_after_the_if() {
        // v1 ceiling: `if p == null { return 0; }` doesn't narrow the
        // statements after the if — only an else branch is narrowed.
        let d = diags(
            "refstruct P { x: int }\n\
             fun f(p: P?): int { if p == null { return 0; } return p.x; }",
        );
        assert!(d.iter().any(|e| e.message.contains("may be null")), "{d:?}");
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
}
