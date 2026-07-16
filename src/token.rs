use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Fun,
    Struct,
    RefStruct,
    Var,
    Const,
    Return,
    Break,
    Continue,
    If,
    Else,
    While,
    For,
    In,
    Import,
    Export,
    From,
    True,
    False,
    Null,
    /// `error` — error declarations, `error.Name` literals, and the
    /// one-word error-code type (ADR 0034).
    ErrorKw,
    /// Reserved for `try` propagation (ADR 0034).
    Try,
    /// `enum` — payload-enum declarations (ADR 0036).
    Enum,
    /// `match` — variant dispatch (ADR 0036).
    Match,
    // Type keywords
    IntType,
    FloatType,
    BoolType,
    StringType,
    FileType,
    // Literals & identifiers
    Identifier(String),
    IntLiteral(i64),
    FloatLiteral(f64),
    StringLiteral(String),
    /// Template-literal text runs (ADR 0030): `` `text${ ``, `}text${`,
    /// and `` }text` ``. A template with no interpolation lexes as a
    /// plain `StringLiteral`.
    TemplateHead(String),
    TemplateMiddle(String),
    TemplateTail(String),
    // Punctuation
    Colon,
    Semicolon,
    Comma,
    Dot,
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    // Operators
    Equals,
    EqEq,
    Plus,
    Minus,
    Asterisk,
    Slash,
    Percent,
    Bang,
    BangEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    AmpAmp,
    PipePipe,
    LeftBracket,
    RightBracket,
    /// `?` — only valid as a postfix type modifier (`int?`).
    Question,
    /// `??` — null coalescing.
    QuestionQuestion,
    /// `?.` — optional chaining.
    QuestionDot,
    // End of input
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
