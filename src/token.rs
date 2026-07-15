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
    // Type keywords
    IntType,
    FloatType,
    BoolType,
    StringType,
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
