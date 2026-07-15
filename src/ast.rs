//! The syntax tree the parser produces and every later phase consumes.
//! Plain data: items (functions, structs, imports), statements, and
//! expressions, each carrying the span diagnostics point at. The
//! `sexpr` rendering exists only for shape assertions in tests.

use crate::span::Span;
use crate::syntax;

pub type Ast = Vec<Item>;

#[derive(Debug, PartialEq)]
pub enum Item {
    Function(Function),
    Struct(Struct),
    Import(ImportDecl),
    Error(ErrorDecl),
}

/// `error NotFound, Timeout;` — module-scoped error codes (ADR 0034).
/// Each name keeps its span so duplicate/resolution diagnostics can
/// point at the exact identifier.
#[derive(Debug, PartialEq)]
pub struct ErrorDecl {
    pub exported: bool,
    pub names: Vec<(String, Span)>,
    pub span: Span,
}

/// `import { a, b } from "./path";` — each name keeps its own span so
/// resolution errors can point at the exact identifier.
#[derive(Debug, PartialEq)]
pub struct ImportDecl {
    pub names: Vec<(String, Span)>,
    pub path: String,
    pub path_span: Span,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Function {
    pub exported: bool,
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Option<TypeAnn>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeAnn,
}

#[derive(Debug, PartialEq)]
pub struct Struct {
    pub exported: bool,
    /// True for `refstruct` — reference semantics (shared, aliased) instead
    /// of the default value semantics (copied).
    pub by_ref: bool,
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: TypeAnn,
}

#[derive(Debug, PartialEq, Clone)]
pub enum TypeAnn {
    Int,
    Float,
    Bool,
    Str,
    File,
    Named(String),
    /// `error` — the one-word error-code type (ADR 0034).
    ErrCode,
    /// `T?` — T or null.
    Optional(Box<TypeAnn>),
    /// `T[]` — a growable array of T, reference semantics like refstruct.
    Array(Box<TypeAnn>),
}

#[derive(Debug, PartialEq)]
pub enum Stmt {
    /// `var`/`const` binding. `mutable` is true for `var`. The annotation is
    /// optional (`var x: Node? = null;`) — required only when the
    /// initializer alone can't name the type (a bare `null`).
    Let {
        mutable: bool,
        name: String,
        ty: Option<TypeAnn>,
        value: Expr,
        span: Span,
    },
    /// `target = value;` — target is a place: a variable or a field chain
    /// rooted at one (`x`, `p.x`, `o.i.v`). The parser rejects anything else.
    Assign {
        target: Expr,
        value: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    /// `break;` — exits the innermost enclosing loop (ADR 0019).
    Break {
        span: Span,
    },
    /// `continue;` — skips to the innermost loop's next iteration.
    Continue {
        span: Span,
    },
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        /// `else if …` parses as an else body containing a single nested `If`.
        else_body: Option<Vec<Stmt>>,
        span: Span,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `for x in xs { … }` — iterates an array; `x` is a const binding of
    /// the element type, fresh each iteration. `for [i, x] in xs` also
    /// binds the const int index.
    For {
        index: Option<String>,
        name: String,
        iterable: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    Expr(Expr),
}

/// `Expr::Convert`'s target type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conv {
    Int,
    Float,
    Str,
}

impl Conv {
    /// The surface keyword — the conversion's name in diagnostics.
    pub fn keyword(self) -> &'static str {
        match self {
            Conv::Int => syntax::KW_INT,
            Conv::Float => syntax::KW_FLOAT,
            Conv::Str => syntax::KW_STRING,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Bool(bool, Span),
    Str(String, Span),
    Ident(String, Span),
    Null(Span),
    /// `error.Name` — an error-code literal (ADR 0034).
    ErrorLit(String, Span),
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `int(x)` / `float(x)` / `string(x)` — explicit conversion to the
    /// named type (ADR 0028/0029). The names are type keywords, so the
    /// form is unshadowable.
    Convert {
        to: Conv,
        /// True for a template's `${e}` (ADR 0030): the string identity
        /// passes through instead of being rejected as a no-op.
        implicit: bool,
        arg: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Field {
        base: Box<Expr>,
        name: String,
        /// True for `?.` — null short-circuits instead of erroring.
        optional: bool,
        span: Span,
    },
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    ArrayLit {
        elements: Vec<Expr>,
        span: Span,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `??` — left side unless it's null, then the (lazily evaluated) right.
    Coalesce,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum UnOp {
    Neg,
    Not,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Coalesce => "??",
        }
    }
}

impl UnOp {
    #[cfg(test)] // only the s-expression test-helper needs this
    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "!",
        }
    }
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::For { span, .. } => *span,
            Stmt::Expr(e) => e.span(),
        }
    }
}

impl Expr {
    /// Can this expression be assigned to? Places are variables, plain
    /// field chains, and index expressions rooted at one. `?.` links are
    /// excluded — a target that might not exist can't be written to.
    pub fn is_place(&self) -> bool {
        match self {
            Expr::Ident(..) => true,
            Expr::Field { base, optional, .. } => !optional && base.is_place(),
            Expr::Index { base, .. } => base.is_place(),
            _ => false,
        }
    }

    /// The textual path of a plain place expression (`cur.left` →
    /// "cur.left") — the key format for narrowing facts. Index expressions
    /// yield `None` (element identity is dynamic, so they can't be
    /// narrowed), as do `?.` links and non-places.
    pub fn place_path(&self) -> Option<String> {
        match self {
            Expr::Ident(n, _) => Some(n.clone()),
            Expr::Field {
                base,
                name,
                optional: false,
                ..
            } => Some(format!("{}.{name}", base.place_path()?)),
            _ => None,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::Str(_, s)
            | Expr::Ident(_, s)
            | Expr::Null(s)
            | Expr::ErrorLit(_, s) => *s,
            Expr::Unary { span, .. }
            | Expr::Convert { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Field { span, .. }
            | Expr::StructLit { span, .. }
            | Expr::ArrayLit { span, .. }
            | Expr::Index { span, .. } => *span,
        }
    }

    /// Span-free s-expression rendering, used to assert structure in tests.
    #[cfg(test)]
    pub fn sexpr(&self) -> String {
        match self {
            Expr::Int(n, _) => n.to_string(),
            Expr::Float(f, _) => f.to_string(),
            Expr::Bool(b, _) => b.to_string(),
            Expr::Str(s, _) => format!("{s:?}"),
            Expr::Ident(name, _) => name.clone(),
            Expr::Null(_) => "null".to_string(),
            Expr::ErrorLit(n, _) => format!("error.{n}"),
            Expr::Unary { op, rhs, .. } => format!("({} {})", op.symbol(), rhs.sexpr()),
            Expr::Convert { to, arg, .. } => {
                format!("({} {})", to.keyword(), arg.sexpr())
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                format!("({} {} {})", op.symbol(), lhs.sexpr(), rhs.sexpr())
            }
            Expr::Call { callee, args, .. } => {
                let args: Vec<String> = args.iter().map(Expr::sexpr).collect();
                format!("(call {} {})", callee.sexpr(), args.join(" "))
            }
            Expr::Field {
                base,
                name,
                optional,
                ..
            } => {
                let op = if *optional { "?." } else { "." };
                format!("({op} {} {})", base.sexpr(), name)
            }
            Expr::StructLit { name, fields, .. } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v.sexpr()))
                    .collect();
                format!("(struct {} {})", name, fs.join(" "))
            }
            Expr::ArrayLit { elements, .. } => {
                let es: Vec<String> = elements.iter().map(Expr::sexpr).collect();
                format!("[{}]", es.join(" "))
            }
            Expr::Index { base, index, .. } => {
                format!("(idx {} {})", base.sexpr(), index.sexpr())
            }
        }
    }
}
