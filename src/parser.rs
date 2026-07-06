use crate::ast::{
    Ast, BinOp, Expr, Field, Function, Item, Param, Stmt, Struct, TypeAnn, UnOp,
};
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
        if self.check(&kind) {
            let span = self.peek().span;
            self.bump();
            span
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

    fn parse_type(&mut self) -> TypeAnn {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::IntType => {
                self.bump();
                TypeAnn::Int
            }
            TokenKind::FloatType => {
                self.bump();
                TypeAnn::Float
            }
            TokenKind::Identifier(n) => {
                self.bump();
                TypeAnn::Named(n)
            }
            other => {
                self.error(
                    format!("expected a type, found {}", describe(&other)),
                    tok.span,
                );
                TypeAnn::Int // recovery default
            }
        }
    }

    fn parse_function(&mut self) -> Function {
        let start = self.expect(TokenKind::Fun);
        let name = self.expect_identifier();
        self.expect(TokenKind::LeftParen);
        let mut params = Vec::new();
        while !self.check(&TokenKind::RightParen) && !self.at_eof() {
            let param_name = self.expect_identifier();
            self.expect(TokenKind::Colon);
            let ty = self.parse_type();
            params.push(Param { name: param_name, ty });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RightParen);
        let return_type = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect(TokenKind::LeftBrace);
        let mut body = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            body.push(self.parse_stmt());
        }
        let end = self.expect(TokenKind::RightBrace);
        Function {
            name,
            params,
            return_type,
            body,
            span: start.to(end),
        }
    }

    fn parse_struct(&mut self) -> Struct {
        let start = self.expect(TokenKind::Struct);
        let name = self.expect_identifier();
        self.expect(TokenKind::LeftBrace);
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            let field_name = self.expect_identifier();
            self.expect(TokenKind::Colon);
            let ty = self.parse_type();
            fields.push(Field { name: field_name, ty });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect(TokenKind::RightBrace);
        Struct {
            name,
            fields,
            span: start.to(end),
        }
    }

    /// Recovery: skip to the next statement/item boundary. Always consumes at
    /// least the offending token — recovery that makes no progress would leave
    /// the caller's loop stuck on the same token forever.
    fn synchronize(&mut self) {
        self.bump();
        while !self.at_eof() {
            match self.peek().kind {
                TokenKind::Semicolon => {
                    self.bump();
                    return;
                }
                TokenKind::RightBrace | TokenKind::Fun | TokenKind::Struct => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Pratt / precedence-climbing expression parser. `min_bp` is the binding
    /// power the current expression must exceed to keep absorbing operators.
    pub fn parse_expr(&mut self, min_bp: u8) -> Expr {
        let mut lhs = self.parse_prefix();
        loop {
            // Postfix (call `(`, field `.`) binds tighter than every operator.
            if matches!(self.peek().kind, TokenKind::LeftParen | TokenKind::Dot) {
                if POSTFIX_BP < min_bp {
                    break;
                }
                lhs = self.parse_postfix(lhs);
                continue;
            }
            let Some((prec, op)) = Precedence::of(&self.peek().kind) else {
                break;
            };
            if prec.left_bp() < min_bp {
                break;
            }
            self.bump(); // operator
            let rhs = self.parse_expr(prec.right_bp());
            let span = lhs.span().to(rhs.span());
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
        self.bump();
        let rhs = self.parse_expr(PREFIX_BP);
        let span = tok.span.to(rhs.span());
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
            TokenKind::Identifier(name) => {
                // ponytail: `Ident {` in expression position is a struct literal.
                // Revisit this disambiguation when `if cond { … }` control flow lands.
                if self.check(&TokenKind::LeftBrace) {
                    self.parse_struct_literal(name, tok.span)
                } else {
                    Expr::Ident(name, tok.span)
                }
            }
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

    fn parse_postfix(&mut self, lhs: Expr) -> Expr {
        match self.peek().kind {
            TokenKind::LeftParen => {
                self.bump();
                let mut args = Vec::new();
                while !self.check(&TokenKind::RightParen) && !self.at_eof() {
                    args.push(self.parse_expr(0));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                let end = self.expect(TokenKind::RightParen);
                let span = lhs.span().to(end);
                Expr::Call {
                    callee: Box::new(lhs),
                    args,
                    span,
                }
            }
            TokenKind::Dot => {
                self.bump();
                let field = self.advance();
                let (name, name_span) = match field.kind {
                    TokenKind::Identifier(n) => (n, field.span),
                    other => {
                        self.error(
                            format!("expected a field name, found {}", describe(&other)),
                            field.span,
                        );
                        (String::new(), field.span)
                    }
                };
                let span = lhs.span().to(name_span);
                Expr::Field {
                    base: Box::new(lhs),
                    name,
                    span,
                }
            }
            _ => unreachable!("parse_postfix called on a non-postfix token"),
        }
    }

    fn parse_struct_literal(&mut self, name: String, start: Span) -> Expr {
        self.expect(TokenKind::LeftBrace);
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            let field_name = self.expect_identifier();
            self.expect(TokenKind::Colon);
            let value = self.parse_expr(0);
            fields.push((field_name, value));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect(TokenKind::RightBrace);
        Expr::StructLit {
            name,
            fields,
            span: start.to(end),
        }
    }

    pub fn parse_stmt(&mut self) -> Stmt {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Var | TokenKind::Const => {
                let mutable = matches!(tok.kind, TokenKind::Var);
                self.bump();
                let name = self.expect_identifier();
                self.expect(TokenKind::Equals);
                let value = self.parse_expr(0);
                let end = self.expect(TokenKind::Semicolon);
                Stmt::Let {
                    mutable,
                    name,
                    value,
                    span: tok.span.to(end),
                }
            }
            TokenKind::Return => {
                self.bump();
                let value = if self.check(&TokenKind::Semicolon) {
                    None
                } else {
                    Some(self.parse_expr(0))
                };
                let end = self.expect(TokenKind::Semicolon);
                Stmt::Return {
                    value,
                    span: tok.span.to(end),
                }
            }
            _ => {
                let expr = self.parse_expr(0);
                // `ident = expr;` is an assignment; anything else is an expression statement.
                if let Expr::Ident(name, ident_span) = &expr {
                    if self.check(&TokenKind::Equals) {
                        let name = name.clone();
                        let start = *ident_span;
                        self.bump(); // '='
                        let value = self.parse_expr(0);
                        let end = self.expect(TokenKind::Semicolon);
                        return Stmt::Assign {
                            name,
                            value,
                            span: start.to(end),
                        };
                    }
                }
                self.expect(TokenKind::Semicolon);
                Stmt::Expr(expr)
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

/// Postfix (call `(`, field `.`) binds tighter than everything, including prefix.
const POSTFIX_BP: u8 = 14;

fn describe(kind: &TokenKind) -> &'static str {
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
}

/// Parses a token stream (which must end in `Eof`) into top-level items,
/// collecting diagnostics and recovering at item boundaries instead of failing
/// on the first error.
pub fn parse(tokens: &[Token]) -> (Ast, Vec<Diagnostic>) {
    let mut parser = Parser {
        tokens,
        pos: 0,
        diagnostics: Vec::new(),
    };
    let mut items = Vec::new();
    while !parser.at_eof() {
        match parser.peek().kind {
            TokenKind::Fun => items.push(Item::Function(parser.parse_function())),
            TokenKind::Struct => items.push(Item::Struct(parser.parse_struct())),
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

    #[test]
    fn function_call() {
        assert_eq!(expr("f(a, b)").sexpr(), "(call f a b)");
    }

    #[test]
    fn field_access_chains_left() {
        assert_eq!(expr("a.b.c").sexpr(), "(. (. a b) c)");
    }

    #[test]
    fn call_then_field_access() {
        assert_eq!(expr("f(x).y").sexpr(), "(. (call f x) y)");
    }

    #[test]
    fn struct_literal() {
        assert_eq!(
            expr("Point { x: 1, y: 2 }").sexpr(),
            "(struct Point x=1 y=2)"
        );
    }

    fn stmt(src: &str) -> Stmt {
        let (tokens, diags) = lex(src);
        assert!(diags.is_empty(), "lex errors: {diags:?}");
        let mut p = Parser {
            tokens: &tokens,
            pos: 0,
            diagnostics: Vec::new(),
        };
        let s = p.parse_stmt();
        assert!(p.diagnostics.is_empty(), "parse errors: {:?}", p.diagnostics);
        s
    }

    #[test]
    fn const_binding_is_immutable() {
        match stmt("const x = a + 1;") {
            Stmt::Let { mutable, name, value, .. } => {
                assert!(!mutable);
                assert_eq!(name, "x");
                assert_eq!(value.sexpr(), "(+ a 1)");
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn var_binding_is_mutable() {
        match stmt("var y = 2;") {
            Stmt::Let { mutable, .. } => assert!(mutable),
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn return_with_and_without_value() {
        match stmt("return a * b;") {
            Stmt::Return { value: Some(e), .. } => assert_eq!(e.sexpr(), "(* a b)"),
            other => panic!("expected Return, got {other:?}"),
        }
        match stmt("return;") {
            Stmt::Return { value: None, .. } => {}
            other => panic!("expected empty Return, got {other:?}"),
        }
    }

    #[test]
    fn assignment_statement() {
        match stmt("x = 5;") {
            Stmt::Assign { name, value, .. } => {
                assert_eq!(name, "x");
                assert_eq!(value.sexpr(), "5");
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn expression_statement() {
        match stmt("f(x);") {
            Stmt::Expr(e) => assert_eq!(e.sexpr(), "(call f x)"),
            other => panic!("expected Expr statement, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_function() {
        let (tokens, ld) = lex("fun add(a: int, b: int): int { return a + b; }");
        assert!(ld.is_empty());
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        assert_eq!(ast.len(), 1);
        match &ast[0] {
            Item::Function(f) => {
                assert_eq!(f.name, "add");
                assert_eq!(f.params.len(), 2);
                assert_eq!(f.return_type, Some(TypeAnn::Int));
                assert_eq!(f.body.len(), 1);
            }
            other => panic!("expected function, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_struct() {
        let (tokens, _) = lex("struct Point { x: int, y: float }");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        match &ast[0] {
            Item::Struct(s) => {
                assert_eq!(s.name, "Point");
                assert_eq!(s.fields.len(), 2);
                assert_eq!(s.fields[1].ty, TypeAnn::Float);
            }
            other => panic!("expected struct, got {other:?}"),
        }
    }

    #[test]
    fn recovers_from_a_bad_top_level_token() {
        // A stray token is reported, then parsing resumes at `fun`.
        let (tokens, _) = lex("42 fun ok(): int { return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1);
        assert_eq!(ast.len(), 1);
    }

    #[test]
    fn recovers_from_a_stray_top_level_right_brace() {
        // Regression: `synchronize()` used to return without consuming a
        // stray `}`, leaving `parse()` stuck on the same token forever.
        let (tokens, _) = lex("} fun ok(): int { return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1);
        assert_eq!(ast.len(), 1);
    }
}
