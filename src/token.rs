use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Fun,
    Struct,
    Var,
    Const,
    Return,
    // Type keywords
    IntType,
    FloatType,
    // Literals & identifiers
    Identifier(String),
    IntLiteral(i64),
    FloatLiteral(f64),
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
    Plus,
    Minus,
    Asterisk,
    Slash,
    Percent,
    Bang,
    Less,
    Greater,
    AmpAmp,
    PipePipe,
    // End of input
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
