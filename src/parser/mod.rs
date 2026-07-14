use crate::ast::{
    Ast, BinOp, Expr, Field, Function, ImportDecl, Item, Param, Stmt, Struct, TypeAnn, UnOp,
};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::token::{Token, TokenKind};

pub struct Parser<'a> {
    pub tokens: &'a [Token],
    pub pos: usize,
    pub diagnostics: Vec<Diagnostic>,
    /// False while parsing an `if`/`while` condition, where `x { … }` must
    /// read as identifier-then-block, not a struct literal. Parentheses
    /// re-enable it.
    pub struct_literals_allowed: bool,
    /// Current expression/statement nesting depth (see `MAX_NESTING`).
    pub depth: u32,
    /// Chain-built expression nodes in the current function (see `MAX_FN_OPS`).
    fn_ops: u32,
    /// True once the current function's operator budget was reported.
    fn_ops_reported: bool,
}

/// Recursion ceiling for nested expressions and statements — a stack-safety
/// guard for the parser's own recursion, not a language limit. Pathological
/// nesting (deep parens, huge `else if` chains) gets a diagnostic instead of
/// a stack overflow. Operator/postfix chains parse at constant depth, so the
/// AST height they build is bounded separately, by `MAX_FN_OPS`.
const MAX_NESTING: u32 = 128;

/// Chain-built nodes (binary operators and `.`/`[]`/`()` links) allowed per
/// function. Chains grow AST height without nesting the parser, and every
/// later pass — checker, narrowing, codegen, even drop glue — recurses per
/// level of that height. Bounding it here, at construction, protects them
/// all at once. Sized well under the interpreter's 65_536-unit eval budget
/// (so anything the parser admits also runs) and far under what the
/// pipeline worker stack fits (main.rs).
const MAX_FN_OPS: u32 = 32_768;

mod exprs;
mod items;
mod stmts;
#[cfg(test)]
mod tests;

/// The cursor and error machinery every grammar layer shares: peeking,
/// consuming, expecting tokens, and the two safety budgets (recursion
/// nesting, per-function operator count).
impl<'a> Parser<'a> {
    /// Creates a parser over a token stream (which must end in `Eof`).
    pub fn new(tokens: &'a [Token]) -> Parser<'a> {
        Parser {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            struct_literals_allowed: true,
            depth: 0,
            fn_ops: 0,
            fn_ops_reported: false,
        }
    }

    /// Claims budget for one chain-built node. On overflow: reports once
    /// per function and returns false — the caller keeps its unwrapped
    /// lhs, freezing the tree's growth while parsing still consumes
    /// tokens normally, so exactly one diagnostic surfaces.
    fn claim_op(&mut self, span: Span) -> bool {
        if self.fn_ops < MAX_FN_OPS {
            self.fn_ops += 1;
            return true;
        }
        if !self.fn_ops_reported {
            self.fn_ops_reported = true;
            self.error(
                format!(
                    "function exceeds {MAX_FN_OPS} operators — split it into smaller functions"
                ),
                span,
            );
        }
        false
    }

    /// Claims one level of nesting. On overflow: reports a diagnostic,
    /// consumes a token (guaranteeing progress), and returns false so the
    /// caller bails out with a placeholder.
    fn enter_nested(&mut self) -> bool {
        if self.depth >= MAX_NESTING {
            let span = self.peek().span;
            self.error(format!("nesting exceeds {MAX_NESTING} levels"), span);
            self.bump();
            false
        } else {
            self.depth += 1;
            true
        }
    }

    fn peek(&self) -> &Token {
        // Safe: the token stream always ends with an Eof sentinel.
        &self.tokens[self.pos]
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Steps past the current token without cloning it. Use when the token's
    /// value is not needed; `advance` returns the (cloned) token instead.
    fn bump(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        self.bump();
        tok
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(kind)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Span {
        self.expect_or_flag(kind).0
    }

    /// Like `expect`, but also reports whether the token was actually there —
    /// statement parsers use the flag to tell their caller they terminated
    /// cleanly (so recovery is driven by outcome, not token-state guessing).
    fn expect_or_flag(&mut self, kind: TokenKind) -> (Span, bool) {
        if self.check(&kind) {
            let span = self.peek().span;
            self.bump();
            (span, true)
        } else {
            let tok = self.peek().clone();
            self.error(
                format!(
                    "expected {}, found {}",
                    describe(&kind),
                    describe(&tok.kind)
                ),
                tok.span,
            );
            (tok.span, false)
        }
    }

    /// `expect` variants that signal failure instead of recovering in
    /// place — for multi-token headers where each follow-on expect would
    /// otherwise re-report the same bad token.
    fn header_identifier(&mut self) -> Option<String> {
        match &self.peek().kind {
            TokenKind::Identifier(_) => Some(self.expect_identifier()),
            _ => {
                self.expect_identifier();
                None
            }
        }
    }

    fn header_token(&mut self, kind: TokenKind) -> Option<Span> {
        let (span, clean) = self.expect_or_flag(kind);
        clean.then_some(span)
    }

    fn expect_identifier(&mut self) -> String {
        let tok = self.peek().clone();
        if let TokenKind::Identifier(name) = tok.kind {
            self.bump();
            name
        } else {
            self.error(
                format!("expected an identifier, found {}", describe(&tok.kind)),
                tok.span,
            );
            String::new()
        }
    }

    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
    }
}

/// Operator precedence, lowest to highest. Declaration order *is* the ranking —
/// the Pratt binding powers are derived from it, so adding a level means adding
/// a variant in the right place, not hand-tuning numbers.
#[derive(Clone, Copy)]
pub(super) enum Precedence {
    Coalesce,   // ??
    Or,         // ||
    And,        // &&
    Equality,   // == !=
    Comparison, // < <= > >=
    Sum,        // + -
    Product,    // * / %
}

impl Precedence {
    /// The precedence and AST operator for an infix token, or `None` if the
    /// token is not a binary operator.
    pub(super) fn of(kind: &TokenKind) -> Option<(Precedence, BinOp)> {
        Some(match kind {
            TokenKind::QuestionQuestion => (Precedence::Coalesce, BinOp::Coalesce),
            TokenKind::PipePipe => (Precedence::Or, BinOp::Or),
            TokenKind::AmpAmp => (Precedence::And, BinOp::And),
            TokenKind::EqEq => (Precedence::Equality, BinOp::Eq),
            TokenKind::BangEq => (Precedence::Equality, BinOp::Ne),
            TokenKind::Less => (Precedence::Comparison, BinOp::Lt),
            TokenKind::LessEq => (Precedence::Comparison, BinOp::Le),
            TokenKind::Greater => (Precedence::Comparison, BinOp::Gt),
            TokenKind::GreaterEq => (Precedence::Comparison, BinOp::Ge),
            TokenKind::Plus => (Precedence::Sum, BinOp::Add),
            TokenKind::Minus => (Precedence::Sum, BinOp::Sub),
            TokenKind::Asterisk => (Precedence::Product, BinOp::Mul),
            TokenKind::Slash => (Precedence::Product, BinOp::Div),
            TokenKind::Percent => (Precedence::Product, BinOp::Rem),
            _ => return None,
        })
    }

    /// Left binding power — higher binds tighter. Scaled by 2 to leave a slot for
    /// the right binding power; left-associative means `right = left + 1`.
    const fn left_bp(self) -> u8 {
        (self as u8 + 1) * 2
    }

    const fn right_bp(self) -> u8 {
        self.left_bp() + 1
    }
}

/// Unary prefix operators bind tighter than every binary operator.
pub(super) const PREFIX_BP: u8 = 16;

/// Postfix (call `(`, field `.`) binds tighter than everything, including prefix.
pub(super) const POSTFIX_BP: u8 = 18;

// Adding a Precedence level shifts the derived binding powers — this guard
// keeps prefix/postfix above every infix level at compile time.
const _: () = assert!(PREFIX_BP > Precedence::Product.right_bp() && POSTFIX_BP > PREFIX_BP);

pub(super) fn describe(kind: &TokenKind) -> &'static str {
    use TokenKind::*;
    match kind {
        Fun => "'fun'",
        Struct => "'struct'",
        RefStruct => "'refstruct'",
        Var => "'var'",
        Const => "'const'",
        Return => "'return'",
        Break => "'break'",
        Continue => "'continue'",
        If => "'if'",
        Else => "'else'",
        While => "'while'",
        For => "'for'",
        In => "'in'",
        Import => "'import'",
        Export => "'export'",
        From => "'from'",
        True => "'true'",
        False => "'false'",
        Null => "'null'",
        IntType => "'int'",
        FloatType => "'float'",
        BoolType => "'bool'",
        StringType => "'string'",
        Identifier(_) => "an identifier",
        IntLiteral(_) => "an integer",
        FloatLiteral(_) => "a float",
        StringLiteral(_) => "a string",
        Colon => "':'",
        Semicolon => "';'",
        Comma => "','",
        Dot => "'.'",
        LeftParen => "'('",
        RightParen => "')'",
        LeftBrace => "'{'",
        RightBrace => "'}'",
        Equals => "'='",
        EqEq => "'=='",
        Plus => "'+'",
        Minus => "'-'",
        Asterisk => "'*'",
        Slash => "'/'",
        Percent => "'%'",
        Bang => "'!'",
        BangEq => "'!='",
        Less => "'<'",
        LessEq => "'<='",
        Greater => "'>'",
        GreaterEq => "'>='",
        LeftBracket => "'['",
        RightBracket => "']'",
        Question => "'?'",
        QuestionQuestion => "'??'",
        QuestionDot => "'?.'",
        AmpAmp => "'&&'",
        PipePipe => "'||'",
        Eof => "end of input",
    }
}

/// Parses a token stream (which must end in `Eof`) into top-level items,
/// collecting diagnostics and recovering at item boundaries instead of failing
/// on the first error.
pub fn parse(tokens: &[Token]) -> (Ast, Vec<Diagnostic>) {
    let mut parser = Parser::new(tokens);
    let mut items = Vec::new();
    while !parser.at_eof() {
        match parser.peek().kind {
            TokenKind::Fun => items.push(Item::Function(parser.parse_function(false))),
            TokenKind::Struct | TokenKind::RefStruct => {
                items.push(Item::Struct(parser.parse_struct(false)))
            }
            TokenKind::Import => items.push(Item::Import(parser.parse_import())),
            TokenKind::Export => {
                parser.bump();
                match parser.peek().kind {
                    TokenKind::Fun => items.push(Item::Function(parser.parse_function(true))),
                    TokenKind::Struct | TokenKind::RefStruct => {
                        items.push(Item::Struct(parser.parse_struct(true)))
                    }
                    _ => {
                        let tok = parser.peek().clone();
                        parser.error(
                            format!(
                                "expected 'fun' or 'struct' after 'export', found {}",
                                describe(&tok.kind)
                            ),
                            tok.span,
                        );
                        parser.synchronize();
                    }
                }
            }
            _ => {
                let tok = parser.peek().clone();
                parser.error(
                    format!("expected 'fun' or 'struct', found {}", describe(&tok.kind)),
                    tok.span,
                );
                parser.synchronize();
            }
        }
    }
    (items, parser.diagnostics)
}
