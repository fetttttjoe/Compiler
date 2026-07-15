//! What the backend needs to know about a type: how many 8-byte words
//! its value occupies, whether it is a pointer-shaped handle where 0
//! means `null`, and where each struct field sits (C-style
//! declaration-order layout, ADR 0009). The checker owns the types;
//! this module only maps them onto the machine.

use crate::check::Resolutions;
use crate::types::{StructType, Type};

/// Recursion bound for layout walks — a recursive value struct has
/// infinite size; its values can't exist, so hitting this is diagnostic.
pub(crate) const FUEL: usize = 64;

/// The backend's view of a value: one word (scalars, handles), a str
/// (two-word fat pointer, content equality), a value struct, or a
/// value optional (`{tag, payload}`, ADR 0021).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Word,
    Str,
    Struct { words: usize, no_memcmp: bool },
    Opt { words: usize, no_memcmp: bool },
}

impl Kind {
    pub(crate) fn words(self) -> usize {
        match self {
            Kind::Word => 1,
            Kind::Str => 2,
            Kind::Struct { words, .. } | Kind::Opt { words, .. } => words,
        }
    }
}

/// A reference-shaped checker type: a handle where 0 means `null`, so a
/// `T?` of it is a nullable pointer for free (ADR 0009).
pub(crate) fn ref_shaped(t: &Type, res: &Resolutions) -> bool {
    match t {
        Type::Array(_) | Type::File => true,
        Type::Struct(m, n) => res.structs[&(*m, n.clone())].by_ref,
        _ => false,
    }
}

/// A checker type's backend kind. `None` = not compilable yet (value
/// optionals, float printing aside) or infinite (recursive value struct).
pub(crate) fn kind_of(t: &Type, res: &Resolutions, fuel: usize) -> Option<Kind> {
    match t {
        Type::Int | Type::Bool | Type::Float | Type::File => Some(Kind::Word),
        // An empty literal's unconstrained element ([]): a handle word.
        Type::Unknown => Some(Kind::Word),
        // A nullable handle is a free word; a value payload gets the
        // tag word (ADR 0021). Canonical zeroed nulls keep int?/bool?
        // memcmp-comparable; float (IEEE), str (content equality), and
        // no-memcmp struct payloads are not.
        Type::Optional(inner) => {
            let k = kind_of(inner, res, fuel.checked_sub(1)?)?;
            if ref_shaped(inner, res) {
                return (k == Kind::Word).then_some(Kind::Word);
            }
            let no_memcmp = matches!(inner.as_ref(), Type::Float)
                || matches!(
                    k,
                    Kind::Str
                        | Kind::Struct {
                            no_memcmp: true,
                            ..
                        }
                        | Kind::Opt { .. }
                );
            Some(Kind::Opt {
                words: 1 + k.words(),
                no_memcmp,
            })
        }
        // The array is always a one-word handle; elements of any
        // compilable kind store inline at a compile-time stride
        // (ADR 0023) — the element only needs a kind of its own.
        Type::Array(inner) => {
            kind_of(inner, res, fuel.checked_sub(1)?)?;
            Some(Kind::Word)
        }
        Type::Str => Some(Kind::Str),
        Type::Struct(m, n) => {
            let def = &res.structs[&(*m, n.clone())];
            if def.by_ref {
                return Some(Kind::Word);
            }
            let next = fuel.checked_sub(1)?;
            let mut words = 0;
            let mut no_memcmp = false;
            for (_, ft) in &def.fields {
                no_memcmp |= matches!(ft, Type::Float);
                match kind_of(ft, res, next)? {
                    Kind::Word => words += 1,
                    Kind::Str => {
                        words += 2;
                        no_memcmp = true;
                    }
                    Kind::Struct {
                        words: w,
                        no_memcmp: n,
                    }
                    | Kind::Opt {
                        words: w,
                        no_memcmp: n,
                    } => {
                        words += w;
                        no_memcmp |= n;
                    }
                }
            }
            Some(Kind::Struct { words, no_memcmp })
        }
        _ => None,
    }
}

/// Byte offset of field `index` in `def` — the sum of the sizes before
/// it (C-style declaration-order layout, ADR 0009).
pub(crate) fn offset_of(def: &StructType, index: usize, res: &Resolutions) -> Option<i64> {
    def.fields[..index].iter().try_fold(0, |sum, (_, ft)| {
        Some(sum + 8 * kind_of(ft, res, FUEL)?.words() as i64)
    })
}
