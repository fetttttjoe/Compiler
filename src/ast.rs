use crate::span::Span;

pub type Ast = Vec<Item>;

#[derive(Debug, PartialEq)]
pub enum Item {
    Function(Function),
    Struct(Struct),
}

#[derive(Debug, PartialEq)]
pub struct Function {
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
    Named(String),
}

#[derive(Debug, PartialEq)]
pub enum Stmt {
    /// `var`/`const` binding. `mutable` is true for `var`.
    Let {
        mutable: bool,
        name: String,
        value: Expr,
        span: Span,
    },
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
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
    Expr(Expr),
}

#[derive(Debug, PartialEq)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Bool(bool, Span),
    Str(String, Span),
    Ident(String, Span),
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
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
        span: Span,
    },
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
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

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::Str(_, s)
            | Expr::Ident(_, s) => *s,
            Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Field { span, .. }
            | Expr::StructLit { span, .. } => *span,
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
            Expr::Unary { op, rhs, .. } => format!("({} {})", op.symbol(), rhs.sexpr()),
            Expr::Binary { op, lhs, rhs, .. } => {
                format!("({} {} {})", op.symbol(), lhs.sexpr(), rhs.sexpr())
            }
            Expr::Call { callee, args, .. } => {
                let args: Vec<String> = args.iter().map(Expr::sexpr).collect();
                format!("(call {} {})", callee.sexpr(), args.join(" "))
            }
            Expr::Field { base, name, .. } => format!("(. {} {})", base.sexpr(), name),
            Expr::StructLit { name, fields, .. } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v.sexpr()))
                    .collect();
                format!("(struct {} {})", name, fs.join(" "))
            }
        }
    }
}
