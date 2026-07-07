use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Fun,
    Struct,
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
    // End of input
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
