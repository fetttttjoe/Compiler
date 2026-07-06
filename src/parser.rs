use crate::ast::{BinOp, Expr, UnOp};
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::token::{Token, TokenKind};

pub struct Parser<'a> {
    pub tokens: &'a [Token],
    pub pos: usize,
    pub diagnostics: Vec<Diagnostic>,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        // Safe: the token stream always ends with an Eof sentinel.
        &self.tokens[self.pos]
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(kind)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Span {
        if self.check(&kind) {
            self.advance().span
        } else {
            let tok = self.peek().clone();
            self.error(
                format!("expected {}, found {}", describe(&kind), describe(&tok.kind)),
                tok.span,
            );
            tok.span
        }
    }

    fn expect_identifier(&mut self) -> String {
        let tok = self.peek().clone();
        if let TokenKind::Identifier(name) = tok.kind {
            self.advance();
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

    /// Recovery: skip to the next statement/item boundary.
    fn synchronize(&mut self) {
        while !self.at_eof() {
            match self.peek().kind {
                TokenKind::Semicolon => {
                    self.advance();
                    return;
                }
                TokenKind::RightBrace | TokenKind::Fun | TokenKind::Struct => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Pratt / precedence-climbing expression parser. `min_bp` is the binding
    /// power the current expression must exceed to keep absorbing operators.
    pub fn parse_expr(&mut self, min_bp: u8) -> Expr {
        let mut lhs = self.parse_prefix();
        loop {
            let Some((prec, op)) = Precedence::of(&self.peek().kind) else {
                break;
            };
            if prec.left_bp() < min_bp {
                break;
            }
            self.advance(); // operator
            let rhs = self.parse_expr(prec.right_bp());
            let span = Span::new(lhs.span().start, rhs.span().end);
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        lhs
    }

    fn parse_prefix(&mut self) -> Expr {
        let tok = self.peek().clone();
        let op = match tok.kind {
            TokenKind::Minus => UnOp::Neg,
            TokenKind::Bang => UnOp::Not,
            _ => return self.parse_atom(),
        };
        self.advance();
        let rhs = self.parse_expr(PREFIX_BP);
        let span = Span::new(tok.span.start, rhs.span().end);
        Expr::Unary {
            op,
            rhs: Box::new(rhs),
            span,
        }
    }

    fn parse_atom(&mut self) -> Expr {
        let tok = self.advance();
        match tok.kind {
            TokenKind::IntLiteral(n) => Expr::Int(n, tok.span),
            TokenKind::FloatLiteral(f) => Expr::Float(f, tok.span),
            TokenKind::Identifier(name) => Expr::Ident(name, tok.span),
            TokenKind::LeftParen => {
                let inner = self.parse_expr(0);
                self.expect(TokenKind::RightParen);
                inner
            }
            other => {
                self.error(
                    format!("expected an expression, found {}", describe(&other)),
                    tok.span,
                );
                Expr::Int(0, tok.span) // recovery placeholder
            }
        }
    }
}

/// Operator precedence, lowest to highest. Declaration order *is* the ranking —
/// the Pratt binding powers are derived from it, so adding a level means adding
/// a variant in the right place, not hand-tuning numbers.
#[derive(Clone, Copy)]
enum Precedence {
    Or,         // ||
    And,        // &&
    Comparison, // < >
    Sum,        // + -
    Product,    // * / %
}

impl Precedence {
    /// The precedence and AST operator for an infix token, or `None` if the
    /// token is not a binary operator.
    fn of(kind: &TokenKind) -> Option<(Precedence, BinOp)> {
        Some(match kind {
            TokenKind::PipePipe => (Precedence::Or, BinOp::Or),
            TokenKind::AmpAmp => (Precedence::And, BinOp::And),
            TokenKind::Less => (Precedence::Comparison, BinOp::Lt),
            TokenKind::Greater => (Precedence::Comparison, BinOp::Gt),
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
    fn left_bp(self) -> u8 {
        (self as u8 + 1) * 2
    }

    fn right_bp(self) -> u8 {
        self.left_bp() + 1
    }
}

/// Unary prefix operators bind tighter than every binary operator.
const PREFIX_BP: u8 = 12;

fn describe(kind: &TokenKind) -> String {
    use TokenKind::*;
    match kind {
        Fun => "'fun'",
        Struct => "'struct'",
        Var => "'var'",
        Const => "'const'",
        Return => "'return'",
        IntType => "'int'",
        FloatType => "'float'",
        Identifier(_) => "an identifier",
        IntLiteral(_) => "an integer",
        FloatLiteral(_) => "a float",
        Colon => "':'",
        Semicolon => "';'",
        Comma => "','",
        Dot => "'.'",
        LeftParen => "'('",
        RightParen => "')'",
        LeftBrace => "'{'",
        RightBrace => "'}'",
        Equals => "'='",
        Plus => "'+'",
        Minus => "'-'",
        Asterisk => "'*'",
        Slash => "'/'",
        Percent => "'%'",
        Bang => "'!'",
        Less => "'<'",
        Greater => "'>'",
        AmpAmp => "'&&'",
        PipePipe => "'||'",
        Eof => "end of input",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    /// Parse a single expression from source (used to drive the Pratt tests).
    fn expr(src: &str) -> Expr {
        let (tokens, diags) = lex(src);
        assert!(diags.is_empty(), "lex errors: {diags:?}");
        let mut p = Parser {
            tokens: &tokens,
            pos: 0,
            diagnostics: Vec::new(),
        };
        let e = p.parse_expr(0);
        assert!(p.diagnostics.is_empty(), "parse errors: {:?}", p.diagnostics);
        e
    }

    #[test]
    fn product_binds_tighter_than_sum() {
        assert_eq!(expr("a - b * c").sexpr(), "(- a (* b c))");
    }

    #[test]
    fn sum_is_left_associative() {
        assert_eq!(expr("a - b - c").sexpr(), "(- (- a b) c)");
    }

    #[test]
    fn unary_binds_tighter_than_product() {
        assert_eq!(expr("-a * b").sexpr(), "(* (- a) b)");
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(expr("(a - b) * c").sexpr(), "(* (- a b) c)");
    }
}
