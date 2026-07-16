# ADR 0035 — Generics: Monomorphization in the Checker

- **Status:** Accepted
- **Date:** 2026-07-16
- **Extends:** 0012 (monomorphization is law), 0017 (Phase D), 0034
  (deferred `Result<T, E>`-shaped code on this)

## Context

Phase D names generics as a big rock, and ADR 0012 settled the strategy
before the feature existed: monomorphize, never box. What remains is
surface syntax, inference, where bodies get checked, and where the
expansion lives so that both engines keep producing identical output
without either one learning a new concept.

## Decisions

1. **Surface (TS shape).** `fun max<T>(a: T, b: T): T`,
   `struct Pair<T, U> { … }`, `refstruct Node<T> { … }`. Type
   application in annotations: `Pair<int, string>`. Explicit call-site
   arguments: `max<int>(a, b)`; explicit literal arguments:
   `Pair<int, string> { … }`. Type parameters are plain identifiers
   scoped to their item; a parameter that shadows a visible type name
   is an error (nothing implicit, ADR 0005). In expression position
   `ident <` is ambiguous with comparison: the parser speculatively
   parses a type-argument list and commits only on `>` followed by
   `(` or `{`, otherwise it backtracks to comparison (the TS answer).
   The type-argument parser splits a `>=` token into `>` `=`
   (`var b: Box<int>= x`); nested closers need nothing — ys has no
   shift operator, so `>>` already lexes as two tokens.
   `main` cannot be generic — the entry point is called, never
   instantiated.
2. **Inference from arguments.** `max(1, 2)` binds `T = int` by
   unifying parameter annotations against argument types, unwrapping
   along exactly the edges `fits` lets values flow (`T?` accepts
   `int` or `int?`; arrays stay invariant). A bare `null` or `[]`
   binds nothing. Conflicting bindings, or a parameter left unbound
   once all arguments are seen: diagnostics naming the fix
   (`write max<int>(…)`).
   Return-position-only parameters always need explicit arguments.
3. **Check at instantiation, no bounds.** A generic body is parsed but
   never checked as a template. Each distinct argument list
   instantiates it: clone the AST, substitute the type parameters —
   the clone's annotations carry pre-resolved types (`TypeAnn` gains
   a `Resolved(Type)` variant that `resolve_type` passes through), so
   `T := int[]` or a foreign struct survives with module identity
   intact — and check the clone like any function. Diagnostics carry an
   `in 'max<string>': ` prefix and point at the true source line (see
   4). Bounds can layer on later without breaking this.
4. **Fresh spans via SourceMap re-registration.** Span-keyed tables
   (`expr_types`, `field_slots`, …) demand unique spans per checked
   body. Each function instance re-registers its defining file's text
   in the `SourceMap` and shifts every cloned span by the new base
   delta. Uniqueness is structural, and any diagnostic or runtime
   trap inside an instance resolves to the original file:line:col
   unchanged — error parity (ADR 0022) costs nothing.
5. **Identity is the canonical mangle.** An instance is
   `(defining module, "max<int>")`, the arguments rendered
   canonically: primitives and wrappers structurally (`int?`,
   `P#2[]`), struct arguments qualified by their defining module
   (`P#2`) — same-named structs from different modules must not
   collide, in instance keys or in `Type::Struct` equality (which
   drives `fits`). Struct instances are `Type::Struct(m, canonical)`
   — existing layout, field-slot, printing, and equality machinery
   works on them untouched. Display (diagnostics, printed values)
   strips the `#N` qualifiers — `Pair<int, string> { a: 1, b: x }` —
   via one pretty-printer; `#` can't appear in identifiers, so
   stripping is unambiguous. New goldens only; the frozen corpus is
   unaffected. Assembly labels sanitize `< > , # space`
   deterministically, so mangles never collide there either.
6. **Both engines see only monomorphic code.** The checker owns the
   expansion: a worklist drains instantiation requests (instances may
   request more, including from generic structs' field types), fueled
   by a depth cap of 32 — exceeding it diagnoses runaway expansion
   (`f<T>` calling `f<T[]>`). Outputs land in `Resolutions`:
   - `instances: HashMap<(usize, String), Function>` — owned,
     substituted, respanned bodies. The interpreter's function index
     and codegen's enumeration walk them after the module ASTs;
     generic templates are skipped by both.
   - `call_targets: HashMap<Span, (usize, String)>` — every resolved
     user-function call, keyed by call span (total, like the type
     table). Both engines resolve calls through it; absent means
     builtin. This replaces name-based call resolution — the one
     edit per engine.
   - `sigs` / `structs` gain one entry per instance.
7. **Instances check in their home module.** A template imported from
   another module instantiates against the *defining* module's alias
   maps — its imports, its visible types. Cross-module generics cost
   nothing new.
8. **Out of scope, deliberately:** bounds/constraints vocabulary,
   variance (instances are invariant, like arrays), default type
   arguments, generic error-union payload interplay beyond what
   existing `T!` restrictions already allow. Each returns as its own
   ADR when need pulls.

## Memory and lowering story

There is none, and that is the point: every instance has exactly the
layout and code the same function written by hand at that type would
have. No boxing (ADR 0012 satisfied definitionally), no dictionaries,
no runtime type information. `src/ir/` and `src/codegen.rs` change
only where they enumerate functions and resolve call targets.

## Consequences

**Positive:** zero new concepts below the checker; the differential
contract holds by construction because both engines consume the same
monomorphic expansion. Diagnostics and traps point at real source.
`Result<T, E>`-shaped user types become writable.

**Accepted costs:** code size grows with distinct instantiations
(inherent to monomorphization, accepted since 0012). A generic body
with a type error stays silent until first instantiated — the C++
template model, mitigated by instantiation-site prefixes. Re-checking
per instance re-registers file text in the SourceMap — memory linear
in instantiation count, fine at current program sizes.
