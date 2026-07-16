use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Conv, Expr, Function, Item, Stmt, TypeAnn, UnOp};
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::narrow::{Fact, NarrowFrame, body_effects, condition_facts, covers, diverges};
use crate::source::SourceMap;
use crate::span::Span;
use crate::syntax;
use crate::types::{
    EnumType, FnSig, StructType, Type, eq_comparable, fits, instance_name, is_numeric, poisoned,
    pretty, unconstrained,
};

use generics::{DEPTH_CAP, FnWork, Mono, instantiate_fn, substitute_ann};

/// A per-module view: visible name → the (module, name) that defines it.
pub type Alias = HashMap<String, (usize, String)>;

/// Name-resolution results — the checker's durable output. For every module,
/// each callable name maps to its defining (module, name), locals and imports
/// alike. Consumed by the interpreter (and later codegen).
pub struct Resolutions {
    /// Every struct definition by (defining module, name) — the type
    /// exports ADR 0009 reserved for codegen (field layout and by_ref).
    /// Generic instances live here under their canonical names
    /// (ADR 0035); templates never do.
    pub structs: HashMap<(usize, String), StructType>,
    /// Every enum definition, same keying and instance story
    /// (ADR 0036).
    pub enums: HashMap<(usize, String), EnumType>,
    /// The variant tag behind every enum construction and every match
    /// arm, keyed by the construction's span / the arm's variant span
    /// (ADR 0036) — engines never resolve a variant name.
    pub variant_tags: HashMap<Span, u32>,
    /// Every resolved user-function call, keyed by the call's span —
    /// total, like the type table (ADR 0035). Both engines resolve
    /// calls through it; an absent span means a builtin.
    pub call_targets: HashMap<Span, (usize, String)>,
    /// Monomorphized function bodies by instance key (ADR 0035):
    /// substituted, respanned clones the engines run/lower after the
    /// module ASTs. Templates themselves are never executable.
    pub instances: HashMap<(usize, String), Function>,
    /// Field-access resolution for codegen: each `Field` expr's span maps
    /// to its base struct, field index, and field type. Spans are
    /// file-global offsets, so they key uniquely across the whole program.
    pub field_slots: HashMap<Span, FieldSlot>,
    /// Every function's resolved signature by (defining module, name).
    pub sigs: HashMap<(usize, String), FnSig>,
    /// The type of every checked expression, keyed by its span (spans are
    /// file-global offsets, unique program-wide). Narrowed where narrowing
    /// applies — which is exactly what codegen wants. Codegen never
    /// re-derives a type.
    pub expr_types: HashMap<Span, Type>,
    /// Each annotated `let`'s resolved declared type, keyed by the
    /// statement span — so codegen never resolves an annotation itself.
    pub let_types: HashMap<Span, Type>,
    /// Declared error names in code order — code = index + 2 (tag 0 =
    /// value, 1 = reserved, ADR 0034). Both engines render names
    /// through this table; codes are never observable.
    pub error_names: Vec<String>,
    /// Each `error.Name` literal's interned code, keyed by its span —
    /// the engines never resolve an error name themselves.
    pub error_lits: HashMap<Span, u32>,
}

/// A resolved field access (see `Resolutions::field_slots`).
pub struct FieldSlot {
    pub base: (usize, String),
    pub index: usize,
    pub ty: Type,
}

/// One module's declared names with their export flags. `structs` is
/// the shared TYPE namespace — enums live in it too (ADR 0036), so a
/// struct and an enum can't share a name and imports resolve through
/// one bucket.
struct ModuleNames {
    fns: HashMap<String, bool>,
    structs: HashMap<String, bool>,
    errs: HashMap<String, bool>,
}

/// Static type checking over the whole module graph. Empty diagnostics =
/// well-typed program. `map` is the span authority: generic instances
/// re-register their defining file to claim fresh span ranges
/// (ADR 0035).
pub fn check(graph: &ModuleGraph, map: &mut SourceMap) -> (Resolutions, Vec<Diagnostic>) {
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
    let mut err_aliases: Vec<Alias> = Vec::new();
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
        let mut err_alias: Alias = names[mi]
            .errs
            .keys()
            .map(|n| (n.clone(), (mi, n.clone())))
            .collect();
        for binding in &module.imports {
            let target = &names[binding.target];
            let target_path = &graph.modules[binding.target].path;
            let fn_export = target.fns.get(&binding.name).copied();
            let ty_export = target.structs.get(&binding.name).copied();
            let err_export = target.errs.get(&binding.name).copied();
            if fn_export.is_none() && ty_export.is_none() && err_export.is_none() {
                let exported_names = target
                    .fns
                    .iter()
                    .chain(target.structs.iter())
                    .chain(target.errs.iter())
                    .filter(|&(_, &exported)| exported)
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
            if fn_export != Some(true) && ty_export != Some(true) && err_export != Some(true) {
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
            if fn_alias.contains_key(&binding.name)
                || ty_alias.contains_key(&binding.name)
                || err_alias.contains_key(&binding.name)
            {
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
            if err_export == Some(true) {
                err_alias.insert(binding.name.clone(), (binding.target, binding.name.clone()));
            }
        }
        fn_aliases.push(fn_alias);
        ty_aliases.push(ty_alias);
        err_aliases.push(err_alias);
    }

    // Pass C: resolve signatures and struct layouts through the alias
    // maps; intern error codes deterministically (module index, then
    // declaration order) — codes start at 2 (ADR 0034 tag space).
    // Generic declarations register as templates instead (ADR 0035):
    // their annotations reference type parameters, so signatures and
    // layouts exist only per instance.
    let mut sigs: HashMap<(usize, String), FnSig> = HashMap::new();
    let mut mono = Mono::new();
    let mut error_codes: HashMap<(usize, String), u32> = HashMap::new();
    let mut error_names: Vec<String> = Vec::new();
    // Templates first: a monomorphic signature may apply a generic
    // struct or enum declared anywhere in the graph. Monomorphic enums
    // leave a shell so `Named` resolution can classify enum-vs-struct
    // before pass C fills the variants (declaration order is free).
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            match item {
                Item::Struct(s) if !s.type_params.is_empty() => {
                    check_type_params(&s.type_params, &ty_aliases[mi], &mut diags);
                    mono.struct_templates.insert((mi, s.name.clone()), s);
                }
                Item::Enum(e) if !e.type_params.is_empty() => {
                    check_type_params(&e.type_params, &ty_aliases[mi], &mut diags);
                    mono.enum_templates.insert((mi, e.name.clone()), e);
                }
                Item::Enum(e) => {
                    mono.enums
                        .insert((mi, e.name.clone()), EnumType { variants: vec![] });
                }
                Item::Function(f) if !f.type_params.is_empty() => {
                    check_type_params(&f.type_params, &ty_aliases[mi], &mut diags);
                    if f.name == syntax::ENTRY_FN && mi == 0 {
                        diags.push(Diagnostic::error(
                            format!("'{}' cannot be generic", syntax::ENTRY_FN),
                            f.span,
                        ));
                    }
                    mono.fn_templates.insert((mi, f.name.clone()), f);
                }
                _ => {}
            }
        }
    }
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            match item {
                Item::Struct(s) if s.type_params.is_empty() => {
                    let fields = s
                        .fields
                        .iter()
                        .map(|f| {
                            let mut cx = TypeCx {
                                module: mi,
                                ty_aliases: &ty_aliases,
                                mono: &mut mono,
                                diags: &mut diags,
                            };
                            let ty = resolve_type(&f.ty, &mut cx, s.span);
                            if matches!(ty, Type::ErrUnion(_)) {
                                diags.push(Diagnostic::error(
                                    "error unions in struct fields are not yet supported"
                                        .to_string(),
                                    s.span,
                                ));
                            }
                            (f.name.clone(), ty)
                        })
                        .collect();
                    mono.structs.insert(
                        (mi, s.name.clone()),
                        StructType {
                            fields,
                            by_ref: s.by_ref,
                        },
                    );
                }
                Item::Enum(e) if e.type_params.is_empty() => {
                    let variants = e
                        .variants
                        .iter()
                        .map(|v| {
                            let payloads = v
                                .payloads
                                .iter()
                                .map(|ann| {
                                    let mut cx = TypeCx {
                                        module: mi,
                                        ty_aliases: &ty_aliases,
                                        mono: &mut mono,
                                        diags: &mut diags,
                                    };
                                    let ty = resolve_type(ann, &mut cx, v.span);
                                    if matches!(ty, Type::ErrUnion(_)) {
                                        diags.push(Diagnostic::error(
                                            "error unions in enum payloads are not yet supported"
                                                .to_string(),
                                            v.span,
                                        ));
                                    }
                                    ty
                                })
                                .collect();
                            (v.name.clone(), payloads)
                        })
                        .collect();
                    mono.enums
                        .get_mut(&(mi, e.name.clone()))
                        .expect("shell inserted above")
                        .variants = variants;
                }
                Item::Function(f) if f.type_params.is_empty() => {
                    let sig = resolve_signature(f, mi, &ty_aliases, &mut mono, &mut diags);
                    sigs.insert((mi, f.name.clone()), sig);
                }
                Item::Struct(_) | Item::Function(_) | Item::Enum(_) | Item::Import(_) => {}
                Item::Error(e) => {
                    for (name, _) in &e.names {
                        // Duplicates were diagnosed in collect_names; the
                        // entry guard keeps their codes stable anyway.
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            error_codes.entry((mi, name.clone()))
                        {
                            slot.insert((error_names.len() + 2) as u32);
                            error_names.push(name.clone());
                        }
                    }
                }
            }
        }
    }

    // Pass D: check every monomorphic function body against its
    // module's view. Generic bodies are checked per instance (pass E).
    let paths: Vec<&str> = graph.modules.iter().map(|m| m.path.as_str()).collect();
    let mut field_slots = HashMap::new();
    let mut expr_types = HashMap::new();
    let mut let_types = HashMap::new();
    let mut error_lits = HashMap::new();
    let mut call_targets = HashMap::new();
    let mut variant_tags = HashMap::new();
    for (mi, module) in graph.modules.iter().enumerate() {
        for item in &module.ast {
            if let Item::Function(f) = item
                && f.type_params.is_empty()
            {
                let mut checker = Checker {
                    module: mi,
                    paths: &paths,
                    fn_alias: &fn_aliases[mi],
                    ty_alias: &ty_aliases[mi],
                    ty_aliases: &ty_aliases,
                    err_alias: &err_aliases[mi],
                    error_codes: &error_codes,
                    sigs: &sigs,
                    mono: &mut mono,
                    diagnostics: &mut diags,
                    scopes: Vec::new(),
                    nonnull: Vec::new(),
                    loop_depth: 0,
                    ret: Type::Unit,
                    try_ok: false,
                    inst_depth: 0,
                    field_slots: &mut field_slots,
                    expr_types: &mut expr_types,
                    let_types: &mut let_types,
                    error_lits: &mut error_lits,
                    call_targets: &mut call_targets,
                    variant_tags: &mut variant_tags,
                };
                checker.check_function(f);
            }
        }
    }

    // Pass E: drain the monomorphization worklist (ADR 0035). Each
    // instance re-registers its defining file for a fresh span range,
    // clones the template with parameters substituted, and checks the
    // clone like any function — which may enqueue more work.
    let mut instances: HashMap<(usize, String), Function> = HashMap::new();
    while let Some(FnWork {
        template,
        args,
        depth,
    }) = mono.work.pop()
    {
        let tmpl = mono.fn_templates[&template];
        let ikey = generics::fn_instance_key(&template, &args);
        let bind: HashMap<String, Type> = tmpl
            .type_params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(args.iter().cloned())
            .collect();
        // The instance signature: template annotations substituted,
        // then resolved in the template's own module (decision 7).
        let mi = template.0;
        let params = tmpl
            .params
            .iter()
            .map(|p| {
                let ann = substitute_ann(&p.ty, &bind);
                let mut cx = TypeCx {
                    module: mi,
                    ty_aliases: &ty_aliases,
                    mono: &mut mono,
                    diags: &mut diags,
                };
                let ty = resolve_type(&ann, &mut cx, tmpl.span);
                if matches!(ty, Type::ErrUnion(_)) {
                    diags.push(Diagnostic::error(
                        "error unions in parameters are not yet supported".to_string(),
                        tmpl.span,
                    ));
                }
                ty
            })
            .collect();
        let ret = match &tmpl.return_type {
            Some(t) => {
                let ann = substitute_ann(t, &bind);
                let mut cx = TypeCx {
                    module: mi,
                    ty_aliases: &ty_aliases,
                    mono: &mut mono,
                    diags: &mut diags,
                };
                resolve_type(&ann, &mut cx, tmpl.span)
            }
            None => Type::Unit,
        };
        sigs.insert(ikey.clone(), FnSig { params, ret });
        // The respan: re-register the defining file, shift by the
        // delta. A module whose file never loaded already has its own
        // diagnostic — skip quietly.
        let Some((fname, ftext, fbase)) = map
            .files()
            .iter()
            .find(|f| f.name() == graph.modules[mi].path)
            .map(|f| (f.name().to_string(), f.text().to_string(), f.base()))
        else {
            continue;
        };
        let delta = map.add(fname, ftext) - fbase;
        let inst = instantiate_fn(tmpl, ikey.1.clone(), &bind, delta);
        let before = diags.len();
        let mut checker = Checker {
            module: mi,
            paths: &paths,
            fn_alias: &fn_aliases[mi],
            ty_alias: &ty_aliases[mi],
            ty_aliases: &ty_aliases,
            err_alias: &err_aliases[mi],
            error_codes: &error_codes,
            sigs: &sigs,
            mono: &mut mono,
            diagnostics: &mut diags,
            scopes: Vec::new(),
            nonnull: Vec::new(),
            loop_depth: 0,
            ret: Type::Unit,
            try_ok: false,
            inst_depth: depth,
            field_slots: &mut field_slots,
            expr_types: &mut expr_types,
            let_types: &mut let_types,
            error_lits: &mut error_lits,
            call_targets: &mut call_targets,
            variant_tags: &mut variant_tags,
        };
        checker.check_function(&inst);
        // Instance errors point at the template's true source line;
        // the prefix names which instantiation tripped them.
        for d in diags[before..].iter_mut() {
            d.message = format!("in '{}': {}", pretty(&ikey.1), d.message);
        }
        instances.insert(ikey, inst);
    }

    // Entry rule, owned by the checker so every entry path agrees:
    // `main()` or `main(args: string[])` (ADR 0031) — the interpreter
    // passes the program arguments, compiled main materializes argv.
    if let Some(f) = graph.modules[0].ast.iter().find_map(|item| match item {
        Item::Function(f) if f.name == syntax::ENTRY_FN && f.type_params.is_empty() => Some(f),
        _ => None,
    }) {
        let ok = match f.params.as_slice() {
            [] => true,
            [p] => p.ty == TypeAnn::Array(Box::new(TypeAnn::Str)),
            _ => false,
        };
        if !ok {
            diags.push(Diagnostic::error(
                format!(
                    "'{}' takes no parameters or exactly (args: string[])",
                    syntax::ENTRY_FN
                ),
                f.span,
            ));
        }
    }

    (
        Resolutions {
            structs: mono.structs,
            enums: mono.enums,
            variant_tags,
            call_targets,
            instances,
            field_slots,
            sigs,
            expr_types,
            let_types,
            error_names,
            error_lits,
        },
        diags,
    )
}

/// A monomorphic function's signature, resolved in its own module.
fn resolve_signature(
    f: &Function,
    module: usize,
    ty_aliases: &[Alias],
    mono: &mut Mono,
    diags: &mut Vec<Diagnostic>,
) -> FnSig {
    let params = f
        .params
        .iter()
        .map(|p| {
            let mut cx = TypeCx {
                module,
                ty_aliases,
                mono,
                diags,
            };
            let ty = resolve_type(&p.ty, &mut cx, f.span);
            if matches!(ty, Type::ErrUnion(_)) {
                diags.push(Diagnostic::error(
                    "error unions in parameters are not yet supported".to_string(),
                    f.span,
                ));
            }
            ty
        })
        .collect();
    let ret = match &f.return_type {
        Some(t) => {
            let mut cx = TypeCx {
                module,
                ty_aliases,
                mono,
                diags,
            };
            resolve_type(t, &mut cx, f.span)
        }
        None => Type::Unit,
    };
    FnSig { params, ret }
}

/// Duplicate and shadowing rules for `<T, U>` lists (ADR 0035): names
/// must be distinct and must not shadow a visible type.
fn check_type_params(params: &[(String, Span)], ty_alias: &Alias, diags: &mut Vec<Diagnostic>) {
    for (i, (name, span)) in params.iter().enumerate() {
        if params[..i].iter().any(|(n, _)| n == name) {
            diags.push(Diagnostic::error(
                format!("duplicate type parameter '{name}'"),
                *span,
            ));
        }
        if ty_alias.contains_key(name) {
            diags.push(Diagnostic::error(
                format!("type parameter '{name}' shadows the type '{name}'"),
                *span,
            ));
        }
    }
}

fn collect_names(ast: &[Item], diags: &mut Vec<Diagnostic>) -> ModuleNames {
    let mut names = ModuleNames {
        fns: HashMap::new(),
        structs: HashMap::new(),
        errs: HashMap::new(),
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
            Item::Enum(e) => {
                if names.structs.insert(e.name.clone(), e.exported).is_some() {
                    diags.push(Diagnostic::error(
                        format!("enum '{}' is already defined", e.name),
                        e.span,
                    ));
                }
            }
            Item::Import(_) => {}
            Item::Error(e) => {
                for (name, span) in &e.names {
                    if names.errs.insert(name.clone(), e.exported).is_some() {
                        diags.push(Diagnostic::error(
                            format!("error '{name}' is already declared"),
                            *span,
                        ));
                    }
                }
            }
        }
    }
    names
}

/// Everything annotation resolution needs, bundled: the resolving
/// module, every module's type view, the grow-only generic state, and
/// the diagnostics sink (ADR 0035).
struct TypeCx<'a, 'g> {
    module: usize,
    ty_aliases: &'a [Alias],
    mono: &'a mut Mono<'g>,
    diags: &'a mut Vec<Diagnostic>,
}

fn resolve_type(ann: &TypeAnn, cx: &mut TypeCx, span: Span) -> Type {
    match ann {
        TypeAnn::Int => Type::Int,
        TypeAnn::Float => Type::Float,
        TypeAnn::Bool => Type::Bool,
        TypeAnn::Str => Type::Str,
        TypeAnn::File => Type::File,
        TypeAnn::ErrCode => Type::ErrCode,
        TypeAnn::ErrUnion(inner) => {
            let inner = resolve_type(inner, cx, span);
            if inner == Type::ErrCode {
                cx.diags.push(Diagnostic::error(
                    "'error!' is redundant — 'error' is already the code type".to_string(),
                    span,
                ));
                return Type::Error;
            }
            Type::ErrUnion(Box::new(inner))
        }
        TypeAnn::Optional(inner) => Type::Optional(Box::new(resolve_type(inner, cx, span))),
        TypeAnn::Array(inner) => {
            let inner = resolve_type(inner, cx, span);
            if matches!(inner, Type::ErrUnion(_)) {
                cx.diags.push(Diagnostic::error(
                    "arrays of error unions are not yet supported".to_string(),
                    span,
                ));
                return Type::Error;
            }
            Type::Array(Box::new(inner))
        }
        // The monomorphizer's substitution carrier (ADR 0035).
        TypeAnn::Resolved(t) => t.clone(),
        // `Pair<int, string>` — instantiate the template (ADR
        // 0035/0036); struct and enum templates share the dispatch.
        TypeAnn::Applied(name, args) => {
            let Some(key) = cx.ty_aliases[cx.module].get(name).cloned() else {
                return unknown_type(name, cx, span);
            };
            let args: Vec<Type> = args.iter().map(|a| resolve_type(a, cx, span)).collect();
            if cx.mono.enum_templates.contains_key(&key) {
                instantiate_enum(&key, args, cx, span)
            } else {
                instantiate_struct(&key, args, cx, span)
            }
        }
        TypeAnn::Named(name) => match cx.ty_aliases[cx.module].get(name) {
            Some(key)
                if cx.mono.struct_templates.contains_key(key)
                    || cx.mono.enum_templates.contains_key(key) =>
            {
                cx.diags.push(Diagnostic::error(
                    format!("'{name}' is generic — write '{name}<…>'"),
                    span,
                ));
                Type::Error
            }
            Some(key) if cx.mono.enums.contains_key(key) => Type::Enum(key.0, key.1.clone()),
            Some((m, n)) => Type::Struct(*m, n.clone()),
            None => unknown_type(name, cx, span),
        },
    }
}

fn unknown_type(name: &str, cx: &mut TypeCx, span: Span) -> Type {
    cx.diags.push(
        Diagnostic::error(format!("unknown type '{name}'"), span)
            .suggest(name, cx.ty_aliases[cx.module].keys().map(String::as_str)),
    );
    Type::Error // recovery: already reported
}

/// Instantiates a struct template at `args` (ADR 0035): the canonical
/// key gets a layout entry with substituted field types, resolved in
/// the template's own module. Self-referential templates terminate on
/// the in-progress marker; diverging families (`A<T>` embedding
/// `A<T[]>`) exhaust the depth fuel and diagnose.
fn instantiate_struct(
    tkey: &(usize, String),
    args: Vec<Type>,
    cx: &mut TypeCx,
    span: Span,
) -> Type {
    let Some(tmpl) = cx.mono.struct_templates.get(tkey).copied() else {
        // A monomorphic struct took type arguments.
        cx.diags.push(Diagnostic::error(
            format!("'{}' takes no type arguments", tkey.1),
            span,
        ));
        return Type::Error;
    };
    if args.len() != tmpl.type_params.len() {
        cx.diags.push(Diagnostic::error(
            format!(
                "'{}' expects {} type argument(s), found {}",
                tkey.1,
                tmpl.type_params.len(),
                args.len()
            ),
            span,
        ));
        return Type::Error;
    }
    if args.iter().any(poisoned) {
        return Type::Error; // the argument already reported
    }
    let ikey = (tkey.0, instance_name(&tkey.1, &args));
    if !cx.mono.structs.contains_key(&ikey) {
        if cx.mono.struct_depth >= DEPTH_CAP {
            cx.diags.push(Diagnostic::error(
                format!(
                    "generic instantiation exceeds depth {DEPTH_CAP} — is '{}' expanding forever?",
                    tkey.1
                ),
                span,
            ));
            return Type::Error;
        }
        // In-progress marker before field resolution: self-reference
        // (`Node<T>` holding `Node<T>?`) must find the key and stop.
        cx.mono.structs.insert(
            ikey.clone(),
            StructType {
                fields: Vec::new(),
                by_ref: tmpl.by_ref,
            },
        );
        cx.mono
            .instance_args
            .insert(ikey.clone(), (tkey.clone(), args.clone()));
        let bind: HashMap<String, Type> = tmpl
            .type_params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(args)
            .collect();
        cx.mono.struct_depth += 1;
        let outer_module = std::mem::replace(&mut cx.module, tkey.0);
        let fields: Vec<(String, Type)> = tmpl
            .fields
            .iter()
            .map(|f| {
                let ann = substitute_ann(&f.ty, &bind);
                let ty = resolve_type(&ann, cx, tmpl.span);
                if matches!(ty, Type::ErrUnion(_)) {
                    cx.diags.push(Diagnostic::error(
                        "error unions in struct fields are not yet supported".to_string(),
                        tmpl.span,
                    ));
                }
                (f.name.clone(), ty)
            })
            .collect();
        cx.module = outer_module;
        cx.mono.struct_depth -= 1;
        cx.mono.structs.get_mut(&ikey).expect("marker above").fields = fields;
    }
    Type::Struct(ikey.0, ikey.1)
}

/// Instantiates an enum template at `args` (ADR 0036): the mirror of
/// `instantiate_struct` — shell marker, substituted payload types
/// resolved in the template's module, shared depth fuel.
fn instantiate_enum(tkey: &(usize, String), args: Vec<Type>, cx: &mut TypeCx, span: Span) -> Type {
    let tmpl = cx.mono.enum_templates[tkey];
    if args.len() != tmpl.type_params.len() {
        cx.diags.push(Diagnostic::error(
            format!(
                "'{}' expects {} type argument(s), found {}",
                tkey.1,
                tmpl.type_params.len(),
                args.len()
            ),
            span,
        ));
        return Type::Error;
    }
    if args.iter().any(poisoned) {
        return Type::Error; // the argument already reported
    }
    let ikey = (tkey.0, instance_name(&tkey.1, &args));
    if !cx.mono.enums.contains_key(&ikey) {
        if cx.mono.struct_depth >= DEPTH_CAP {
            cx.diags.push(Diagnostic::error(
                format!(
                    "generic instantiation exceeds depth {DEPTH_CAP} — is '{}' expanding forever?",
                    tkey.1
                ),
                span,
            ));
            return Type::Error;
        }
        cx.mono
            .enums
            .insert(ikey.clone(), EnumType { variants: vec![] });
        cx.mono
            .instance_args
            .insert(ikey.clone(), (tkey.clone(), args.clone()));
        let bind: HashMap<String, Type> = tmpl
            .type_params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(args)
            .collect();
        cx.mono.struct_depth += 1;
        let outer_module = std::mem::replace(&mut cx.module, tkey.0);
        let variants: Vec<(String, Vec<Type>)> = tmpl
            .variants
            .iter()
            .map(|v| {
                let payloads = v
                    .payloads
                    .iter()
                    .map(|ann| {
                        let ann = substitute_ann(ann, &bind);
                        let ty = resolve_type(&ann, cx, v.span);
                        if matches!(ty, Type::ErrUnion(_)) {
                            cx.diags.push(Diagnostic::error(
                                "error unions in enum payloads are not yet supported".to_string(),
                                v.span,
                            ));
                        }
                        ty
                    })
                    .collect();
                (v.name.clone(), payloads)
            })
            .collect();
        cx.module = outer_module;
        cx.mono.struct_depth -= 1;
        cx.mono.enums.get_mut(&ikey).expect("shell above").variants = variants;
    }
    Type::Enum(ikey.0, ikey.1)
}
struct VarInfo {
    ty: Type,
    mutable: bool,
}

struct Checker<'a, 'g> {
    module: usize,
    paths: &'a [&'a str],
    fn_alias: &'a Alias,
    ty_alias: &'a Alias,
    /// Every module's type view — annotation resolution instantiates
    /// generic structs in their defining module (ADR 0035).
    ty_aliases: &'a [Alias],
    err_alias: &'a Alias,
    error_codes: &'a HashMap<(usize, String), u32>,
    sigs: &'a HashMap<(usize, String), FnSig>,
    /// Generic templates, instantiation state, and the struct-layout
    /// table (grow-only, ADR 0035).
    mono: &'a mut Mono<'g>,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<HashMap<String, VarInfo>>,
    /// Narrowing stack: place paths (`cur`, `cur.left`) proven non-null by
    /// an enclosing `!= null` check. A narrowed `T?` reads as `T`. Rebinding
    /// removes a path and its extensions permanently; shadowing only hides
    /// it while the shadow's frame lives; field paths are also dropped on
    /// any call or field write, since aliases can reach them.
    nonnull: Vec<NarrowFrame>,
    /// How many loops enclose the statement being checked — `break`/
    /// `continue` are rejected at depth 0 (ADR 0019).
    loop_depth: usize,
    ret: Type,
    /// True exactly while typing a statement's direct right-hand side —
    /// the only positions where `try` is supported (ADR 0034). Set
    /// ONLY by `type_of_rhs`; taken (and reset) at every
    /// `type_of_expr` entry, so operands never inherit it.
    try_ok: bool,
    /// Instantiation-chain depth of the body being checked — 0 for
    /// source functions; instances carry their chain depth so
    /// transitive requests can hit the cap (ADR 0035).
    inst_depth: u32,
    /// Codegen resolution tables filled in as expressions type (see
    /// `Resolutions::field_slots` / `call_targets`).
    field_slots: &'a mut HashMap<Span, FieldSlot>,
    expr_types: &'a mut HashMap<Span, Type>,
    let_types: &'a mut HashMap<Span, Type>,
    error_lits: &'a mut HashMap<Span, u32>,
    call_targets: &'a mut HashMap<Span, (usize, String)>,
    variant_tags: &'a mut HashMap<Span, u32>,
}

mod exprs;
mod generics;
mod stmts;
#[cfg(test)]
mod tests;

/// Shared plumbing: diagnostics, name rendering, and scope helpers.
impl Checker<'_, '_> {
    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }

    /// Annotation resolution from inside body checking — the checker's
    /// parts, bundled into the shared context (ADR 0035).
    fn resolve(&mut self, ann: &TypeAnn, span: Span) -> Type {
        let mut cx = TypeCx {
            module: self.module,
            ty_aliases: self.ty_aliases,
            mono: self.mono,
            diags: self.diagnostics,
        };
        resolve_type(ann, &mut cx, span)
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
        // Syntax-only, so only an `else`-carrying match counts — the
        // analysis can't prove exhaustiveness (ADR 0036).
        Stmt::Match {
            arms,
            else_body: Some(else_body),
            ..
        } => arms.iter().all(|a| always_returns(&a.body)) && always_returns(else_body),
        _ => false,
    })
}
