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
    /// `T?` — T or null.
    Optional(Box<Type>),
    /// `T[]` — growable array, reference semantics (aliased, identity
    /// equality), like refstruct.
    Array(Box<Type>),
    /// `error` — a declared error code (ADR 0034): one word, identity
    /// equality, module-scoped names.
    ErrCode,
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
        Type::Optional(inner) | Type::Array(inner) => poisoned(inner),
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

impl Type {
    pub fn name(&self) -> String {
        match self {
            Type::Int => "int".to_string(),
            Type::Float => "float".to_string(),
            Type::Bool => "bool".to_string(),
            Type::Str => "string".to_string(),
            Type::File => "file".to_string(),
            Type::Struct(_, n) => n.clone(),
            Type::Optional(inner) => format!("{}?", inner.name()),
            Type::Array(inner) if unconstrained(inner) => "[]".to_string(),
            Type::Array(inner) => format!("{}[]", inner.name()),
            Type::ErrCode => "error".to_string(),
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

/// Comparability for `==`/`!=`: two non-unit types compare when either fits
/// the other — same type, `null` vs `T?`, or `T` vs `T?`.
pub(crate) fn eq_comparable(lt: &Type, rt: &Type) -> bool {
    *lt != Type::Unit && *rt != Type::Unit && (fits(lt, rt) || fits(rt, lt))
}

pub(crate) fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::Int | Type::Float)
}
