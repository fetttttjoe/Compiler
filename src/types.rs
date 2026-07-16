//! The type vocabulary shared by every phase: `Type` itself plus the
//! compatibility algebra (`fits`), the poison/unconstrained recovery
//! predicates, and the resolved signature shapes. The checker produces
//! these; the interpreter and future codegen consume them.

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    /// An open file handle (ADR 0031) — opaque, identity equality,
    /// ref-shaped like refstruct handles so `file?` is a free word.
    File,
    /// A struct type identified by (defining module, name) — same-named
    /// structs in different modules are distinct types.
    Struct(usize, String),
    /// A payload enum (ADR 0036), identified like structs; instances
    /// carry canonical names (ADR 0035).
    Enum(usize, String),
    /// `T?` — T or null.
    Optional(Box<Type>),
    /// `T[]` — growable array, reference semantics (aliased, identity
    /// equality), like refstruct.
    Array(Box<Type>),
    /// `error` — a declared error code (ADR 0034): one word, identity
    /// equality, module-scoped names.
    ErrCode,
    /// `T!` — T or a declared error code (ADR 0034): tag word first
    /// (0 = value, 1 = reserved, ≥2 = the code), payload after.
    ErrUnion(Box<Type>),
    /// The type of the `null` literal; fits only into `T?` slots.
    Null,
    Unit,
    /// Poisoned recovery: an expression that already produced a diagnostic.
    /// Fits everything and compares with everything (structurally — a
    /// `T?`/`T[]` wrapper keeps the property), so one mistake yields one
    /// error instead of a cascade. Never printed in messages.
    Error,
    /// The element type of an empty array literal: nothing constrains it
    /// yet. Unlike `Error`, no diagnostic exists — it fits only array
    /// shapes, and a binding can't keep it (annotation required).
    Unknown,
}

/// Structurally poisoned: a diagnostic was already emitted somewhere inside
/// this type, so every further check stays silent.
pub(crate) fn poisoned(t: &Type) -> bool {
    match t {
        Type::Error => true,
        Type::Optional(inner) | Type::Array(inner) | Type::ErrUnion(inner) => poisoned(inner),
        _ => false,
    }
}

/// An element type that imposes no constraint yet — `[]`, or nested empties
/// like `[[]]`.
pub(crate) fn unconstrained(t: &Type) -> bool {
    match t {
        Type::Unknown => true,
        Type::Array(inner) => unconstrained(inner),
        _ => false,
    }
}

/// The canonical rendering of a type inside instance names (ADR 0035):
/// like `name()`, but struct arguments carry their defining module
/// (`P#2`) so same-named structs from different modules never collide
/// — in instance keys or in `Type::Struct` equality.
pub(crate) fn canon_name(t: &Type) -> String {
    match t {
        Type::Struct(m, n) | Type::Enum(m, n) => format!("{n}#{m}"),
        Type::Optional(inner) => format!("{}?", canon_name(inner)),
        Type::Array(inner) => format!("{}[]", canon_name(inner)),
        Type::ErrUnion(inner) => format!("{}!", canon_name(inner)),
        other => other.name(),
    }
}

/// The canonical instance name for a template applied to `args` — the
/// identity both engines key on (ADR 0035).
pub(crate) fn instance_name(base: &str, args: &[Type]) -> String {
    let parts: Vec<String> = args.iter().map(canon_name).collect();
    format!("{base}<{}>", parts.join(", "))
}

/// Strips the `#N` module qualifiers out of a canonical name for
/// display — `Pair<int, P#2>` renders as `Pair<int, P>`. `#` cannot
/// appear in identifiers, so stripping is unambiguous.
pub(crate) fn pretty(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Type {
    pub fn name(&self) -> String {
        match self {
            Type::Int => "int".to_string(),
            Type::Float => "float".to_string(),
            Type::Bool => "bool".to_string(),
            Type::Str => "string".to_string(),
            Type::File => "file".to_string(),
            // Instances store canonical names; display strips the
            // module qualifiers (ADR 0035) — `pretty` is the identity
            // for source names.
            Type::Struct(_, n) | Type::Enum(_, n) => pretty(n),
            Type::Optional(inner) => format!("{}?", inner.name()),
            Type::Array(inner) if unconstrained(inner) => "[]".to_string(),
            Type::Array(inner) => format!("{}[]", inner.name()),
            Type::ErrCode => "error".to_string(),
            Type::ErrUnion(inner) => format!("{}!", inner.name()),
            Type::Null => "null".to_string(),
            Type::Unit => "unit".to_string(),
            Type::Error => "<error>".to_string(),
            Type::Unknown => "unknown".to_string(),
        }
    }
}

/// Can a value of type `value` be stored where `target` is expected?
/// Exact match, or `T`/`null` into `T?`. Never an implicit conversion —
/// optionality is spelled in the target's type.
pub(crate) fn fits(value: &Type, target: &Type) -> bool {
    if value == target || poisoned(value) || poisoned(target) {
        return true;
    }
    match (value, target) {
        // An unconstrained target (an empty literal's element slot)
        // accepts anything; only transient literals ever have it — a
        // binding can't keep `unknown` (annotation required at `let`).
        (_, Type::Unknown) => true,
        (_, Type::Optional(inner)) => matches!(value, Type::Null) || fits(value, inner),
        // A value or a code flows into `T!` (ADR 0034); `T!` itself only
        // matched exactly above — unions never silently unwrap.
        (_, Type::ErrUnion(inner)) => matches!(value, Type::ErrCode) || fits(value, inner),
        // Arrays are invariant — int[] into int?[] would let an alias push
        // null into the int[] — except unconstrained elements on either
        // side: `[]` fits any array slot and accepts any element type.
        (Type::Array(v), Type::Array(t)) => unconstrained(v) || unconstrained(t) || v == t,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructType {
    pub fields: Vec<(String, Type)>,
    /// True for `refstruct` declarations (reference semantics).
    pub by_ref: bool,
}

/// A resolved enum definition (ADR 0036): variants in declaration
/// order — the tag IS the index — each with its payload types.
#[derive(Debug, Clone, PartialEq)]
pub struct EnumType {
    pub variants: Vec<(String, Vec<Type>)>,
}

/// Comparability for `==`/`!=`: two non-unit types compare when either fits
/// the other — same type, `null` vs `T?`, or `T` vs `T?`. Error unions
/// never compare directly — only against the bare `error` marker, which
/// is handled before this (ADR 0034: narrow first).
pub(crate) fn eq_comparable(lt: &Type, rt: &Type) -> bool {
    !matches!(lt, Type::ErrUnion(_))
        && !matches!(rt, Type::ErrUnion(_))
        && *lt != Type::Unit
        && *rt != Type::Unit
        && (fits(lt, rt) || fits(rt, lt))
}

pub(crate) fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float)
}
