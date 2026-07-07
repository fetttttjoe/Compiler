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
    If,
    Else,
    While,
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
