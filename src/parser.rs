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
}

/// Recursion ceiling for nested expressions and statements — a stack-safety
/// guard, not a language limit. Pathological nesting (deep parens, huge
/// `else if` chains) gets a diagnostic instead of a stack overflow.
const MAX_NESTING: u32 = 128;

impl<'a> Parser<'a> {
    /// Creates a parser over a token stream (which must end in `Eof`).
    pub fn new(tokens: &'a [Token]) -> Parser<'a> {
        Parser {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            struct_literals_allowed: true,
            depth: 0,
        }
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
                format!("expected {}, found {}", describe(&kind), describe(&tok.kind)),
                tok.span,
            );
            (tok.span, false)
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
        let base = self.parse_base_type();
        if self.eat(&TokenKind::Question) {
            TypeAnn::Optional(Box::new(base))
        } else {
            base
        }
    }

    fn parse_base_type(&mut self) -> TypeAnn {
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
            TokenKind::BoolType => {
                self.bump();
                TypeAnn::Bool
            }
            TokenKind::StringType => {
                self.bump();
                TypeAnn::Str
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

    fn parse_function(&mut self, exported: bool) -> Function {
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
        // An unclosed body is recovered by the caller's item-level synchronize.
        let (body, end, _) = self.parse_block();
        Function {
            exported,
            name,
            params,
            return_type,
            body,
            span: start.to(end),
        }
    }

    fn parse_struct(&mut self, exported: bool) -> Struct {
        // The caller dispatched on the keyword — `struct` or `refstruct`.
        let kw = self.advance();
        let by_ref = kw.kind == TokenKind::RefStruct;
        let start = kw.span;
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
            exported,
            by_ref,
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
                TokenKind::RightBrace
                | TokenKind::Fun
                | TokenKind::Struct
                | TokenKind::RefStruct
                | TokenKind::Import
                | TokenKind::Export => return,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// Pratt / precedence-climbing expression parser. `min_bp` is the binding
    /// power the current expression must exceed to keep absorbing operators.
    pub fn parse_expr(&mut self, min_bp: u8) -> Expr {
        if !self.enter_nested() {
            return Expr::Int(0, self.peek().span); // recovery placeholder
        }
        let expr = self.parse_expr_inner(min_bp);
        self.depth -= 1;
        expr
    }

    fn parse_expr_inner(&mut self, min_bp: u8) -> Expr {
        let mut lhs = self.parse_prefix();
        loop {
            // Postfix (call `(`, field `.`/`?.`) binds tighter than every operator.
            if matches!(
                self.peek().kind,
                TokenKind::LeftParen | TokenKind::Dot | TokenKind::QuestionDot
            ) {
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
            TokenKind::True => Expr::Bool(true, tok.span),
            TokenKind::False => Expr::Bool(false, tok.span),
            TokenKind::Null => Expr::Null(tok.span),
            TokenKind::StringLiteral(s) => Expr::Str(s, tok.span),
            TokenKind::Identifier(name) => {
                if self.struct_literals_allowed && self.check(&TokenKind::LeftBrace) {
                    self.parse_struct_literal(name, tok.span)
                } else {
                    Expr::Ident(name, tok.span)
                }
            }
            TokenKind::LeftParen => {
                // Parentheses re-enable struct literals inside a condition.
                let prev = self.struct_literals_allowed;
                self.struct_literals_allowed = true;
                let inner = self.parse_expr(0);
                self.struct_literals_allowed = prev;
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
                // Call parentheses re-enable struct literals inside a
                // condition, same as grouping parentheses.
                let prev = self.struct_literals_allowed;
                self.struct_literals_allowed = true;
                let mut args = Vec::new();
                while !self.check(&TokenKind::RightParen) && !self.at_eof() {
                    args.push(self.parse_expr(0));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.struct_literals_allowed = prev;
                let end = self.expect(TokenKind::RightParen);
                let span = lhs.span().to(end);
                Expr::Call {
                    callee: Box::new(lhs),
                    args,
                    span,
                }
            }
            TokenKind::Dot | TokenKind::QuestionDot => {
                let optional = self.peek().kind == TokenKind::QuestionDot;
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
                    optional,
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

    /// Parses `import { a, b } from "./path";`.
    fn parse_import(&mut self) -> ImportDecl {
        let start = self.expect(TokenKind::Import);
        self.expect(TokenKind::LeftBrace);
        let mut names = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            let span = self.peek().span;
            let name = self.expect_identifier();
            names.push((name, span));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RightBrace);
        self.expect(TokenKind::From);
        let tok = self.peek().clone();
        let (path, path_span) = if let TokenKind::StringLiteral(p) = tok.kind {
            self.bump();
            (p, tok.span)
        } else {
            self.error(
                format!("expected a module path string, found {}", describe(&tok.kind)),
                tok.span,
            );
            (String::new(), tok.span)
        };
        let end = self.expect(TokenKind::Semicolon);
        ImportDecl {
            names,
            path,
            path_span,
            span: start.to(end),
        }
    }

    /// Parses an `if`/`while` condition: struct literals are disallowed so
    /// `if x { … }` reads `x` as the condition, not a struct literal `x {}`.
    fn parse_condition(&mut self) -> Expr {
        let prev = self.struct_literals_allowed;
        self.struct_literals_allowed = false;
        let cond = self.parse_expr(0);
        self.struct_literals_allowed = prev;
        cond
    }

    /// Parses `{ stmt* }`, synchronizing after a malformed statement so one
    /// error doesn't cascade across the rest of the block. Returns the
    /// statements and the span of the closing brace.
    /// Parses `{ stmt* }`. Returns the statements, the closing brace's span,
    /// and whether the block terminated cleanly (closing brace present).
    fn parse_block(&mut self) -> (Vec<Stmt>, Span, bool) {
        self.expect(TokenKind::LeftBrace);
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            let (stmt, clean) = self.parse_stmt();
            stmts.push(stmt);
            // A statement that didn't terminate cleanly left the parser
            // mid-statement — skip to a boundary before continuing.
            if !clean {
                self.synchronize_stmt();
            }
        }
        let (end, closed) = self.expect_or_flag(TokenKind::RightBrace);
        (stmts, end, closed)
    }

    /// Statement-level recovery: skip past the next `;`, or stop before a
    /// token that can start a statement or close the block. A no-op when the
    /// parser is already at such a boundary.
    fn synchronize_stmt(&mut self) {
        while !self.at_eof() {
            match self.peek().kind {
                TokenKind::Semicolon => {
                    self.bump();
                    return;
                }
                TokenKind::RightBrace
                | TokenKind::Var
                | TokenKind::Const
                | TokenKind::Return
                | TokenKind::If
                | TokenKind::While => return,
                _ => self.bump(),
            }
        }
    }

    fn parse_if(&mut self) -> (Stmt, bool) {
        // `else if` chains recurse here directly, so they claim depth too.
        if !self.enter_nested() {
            return (Stmt::Expr(Expr::Int(0, self.peek().span)), false); // recovery placeholder
        }
        let result = self.parse_if_inner();
        self.depth -= 1;
        result
    }

    fn parse_if_inner(&mut self) -> (Stmt, bool) {
        let start = self.peek().span;
        self.bump(); // 'if'
        let cond = self.parse_condition();
        let (then_body, then_end, mut clean) = self.parse_block();
        let mut span = start.to(then_end);
        let mut else_body = None;
        if self.eat(&TokenKind::Else) {
            if self.check(&TokenKind::If) {
                // `else if …` nests as an else body with a single If. (The
                // depth guard can substitute a placeholder, which has no span
                // to extend — keep the chain's span as-is then.)
                let (nested, nested_clean) = self.parse_if();
                clean = nested_clean;
                if let Stmt::If { span: nested_span, .. } = &nested {
                    span = start.to(*nested_span);
                }
                else_body = Some(vec![nested]);
            } else {
                let (body, else_end, else_clean) = self.parse_block();
                clean = else_clean;
                span = start.to(else_end);
                else_body = Some(body);
            }
        }
        (
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            },
            clean,
        )
    }

    /// Parses one statement. The flag reports whether it terminated cleanly
    /// (its `;` or closing `}` was present) — callers synchronize when not.
    pub fn parse_stmt(&mut self) -> (Stmt, bool) {
        if !self.enter_nested() {
            return (Stmt::Expr(Expr::Int(0, self.peek().span)), false); // recovery placeholder
        }
        let result = self.parse_stmt_inner();
        self.depth -= 1;
        result
    }

    fn parse_stmt_inner(&mut self) -> (Stmt, bool) {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::If => self.parse_if(),
            TokenKind::While => {
                self.bump();
                let cond = self.parse_condition();
                let (body, end, clean) = self.parse_block();
                (
                    Stmt::While {
                        cond,
                        body,
                        span: tok.span.to(end),
                    },
                    clean,
                )
            }
            TokenKind::Var | TokenKind::Const => {
                let mutable = matches!(tok.kind, TokenKind::Var);
                self.bump();
                let name = self.expect_identifier();
                let ty = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_type())
                } else {
                    None
                };
                self.expect(TokenKind::Equals);
                let value = self.parse_expr(0);
                let (end, clean) = self.expect_or_flag(TokenKind::Semicolon);
                (
                    Stmt::Let {
                        mutable,
                        name,
                        ty,
                        value,
                        span: tok.span.to(end),
                    },
                    clean,
                )
            }
            TokenKind::Return => {
                self.bump();
                let value = if self.check(&TokenKind::Semicolon) {
                    None
                } else {
                    Some(self.parse_expr(0))
                };
                let (end, clean) = self.expect_or_flag(TokenKind::Semicolon);
                (
                    Stmt::Return {
                        value,
                        span: tok.span.to(end),
                    },
                    clean,
                )
            }
            _ => {
                let expr = self.parse_expr(0);
                // `place = expr;` is an assignment; anything else is an
                // expression statement.
                if self.check(&TokenKind::Equals) {
                    // Only places (a variable or plain field chain) can be
                    // assigned to; `?.` links are excluded by place_path.
                    if expr.place_path().is_none() {
                        self.error("invalid assignment target".to_string(), expr.span());
                    }
                    let start = expr.span();
                    self.bump(); // '='
                    let value = self.parse_expr(0);
                    let (end, clean) = self.expect_or_flag(TokenKind::Semicolon);
                    return (
                        Stmt::Assign {
                            target: expr,
                            value,
                            span: start.to(end),
                        },
                        clean,
                    );
                }
                let (_, clean) = self.expect_or_flag(TokenKind::Semicolon);
                (Stmt::Expr(expr), clean)
            }
        }
    }
}


/// Operator precedence, lowest to highest. Declaration order *is* the ranking —
/// the Pratt binding powers are derived from it, so adding a level means adding
/// a variant in the right place, not hand-tuning numbers.
#[derive(Clone, Copy)]
enum Precedence {
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
    fn of(kind: &TokenKind) -> Option<(Precedence, BinOp)> {
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
const PREFIX_BP: u8 = 16;

/// Postfix (call `(`, field `.`) binds tighter than everything, including prefix.
const POSTFIX_BP: u8 = 18;

// Adding a Precedence level shifts the derived binding powers — this guard
// keeps prefix/postfix above every infix level at compile time.
const _: () = assert!(PREFIX_BP > Precedence::Product.right_bp() && POSTFIX_BP > PREFIX_BP);

fn describe(kind: &TokenKind) -> &'static str {
    use TokenKind::*;
    match kind {
        Fun => "'fun'",
        Struct => "'struct'",
        RefStruct => "'refstruct'",
        Var => "'var'",
        Const => "'const'",
        Return => "'return'",
        If => "'if'",
        Else => "'else'",
        While => "'while'",
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
                    TokenKind::Fun => {
                        items.push(Item::Function(parser.parse_function(true)))
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    /// Parse a single expression from source (used to drive the Pratt tests).
    fn expr(src: &str) -> Expr {
        let (tokens, diags) = lex(src);
        assert!(diags.is_empty(), "lex errors: {diags:?}");
        let mut p = Parser::new(&tokens);
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
        let mut p = Parser::new(&tokens);
        let (s, _clean) = p.parse_stmt();
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
            Stmt::Assign { target, value, .. } => {
                assert_eq!(target.sexpr(), "x");
                assert_eq!(value.sexpr(), "5");
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn field_assignment_statement() {
        match stmt("o.i.v = 1;") {
            Stmt::Assign { target, value, .. } => {
                assert_eq!(target.sexpr(), "(. (. o i) v)");
                assert_eq!(value.sexpr(), "1");
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn optional_chaining_and_coalescing_parse() {
        assert_eq!(expr("a?.b ?? c").sexpr(), "(?? (?. a b) c)");
        assert_eq!(expr("a.b?.c").sexpr(), "(?. (. a b) c)");
    }

    #[test]
    fn coalescing_binds_loosest() {
        assert_eq!(expr("a ?? b || c").sexpr(), "(?? a (|| b c))");
    }

    #[test]
    fn optional_type_annotations_parse() {
        let (tokens, _) = lex("fun f(p: Point?): int? { return 1; }");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        let Item::Function(f) = &ast[0] else { panic!() };
        assert_eq!(
            f.params[0].ty,
            TypeAnn::Optional(Box::new(TypeAnn::Named("Point".into())))
        );
        assert_eq!(f.return_type, Some(TypeAnn::Optional(Box::new(TypeAnn::Int))));
    }

    #[test]
    fn let_bindings_take_optional_annotations() {
        match stmt("var head: Node? = null;") {
            Stmt::Let { ty, value, .. } => {
                assert_eq!(
                    ty,
                    Some(TypeAnn::Optional(Box::new(TypeAnn::Named("Node".into()))))
                );
                assert_eq!(value.sexpr(), "null");
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn optional_chain_is_not_a_place() {
        let (tokens, _) = lex("a?.b = 1;");
        let mut p = Parser::new(&tokens);
        p.parse_stmt();
        assert!(
            p.diagnostics
                .iter()
                .any(|e| e.message.contains("invalid assignment target")),
            "{:?}",
            p.diagnostics
        );
    }

    #[test]
    fn non_place_assignment_target_is_an_error() {
        let (tokens, _) = lex("f() = 1;");
        let mut p = Parser::new(&tokens);
        p.parse_stmt();
        assert!(
            p.diagnostics
                .iter()
                .any(|e| e.message.contains("invalid assignment target")),
            "{:?}",
            p.diagnostics
        );
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
    fn refstruct_sets_the_by_ref_flag() {
        let (tokens, _) = lex("refstruct P { x: int } export refstruct Q { y: int } struct V { z: int }");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        let Item::Struct(p) = &ast[0] else { panic!() };
        assert!(p.by_ref && !p.exported);
        let Item::Struct(q) = &ast[1] else { panic!() };
        assert!(q.by_ref && q.exported);
        let Item::Struct(v) = &ast[2] else { panic!() };
        assert!(!v.by_ref);
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
    fn parses_an_import_declaration() {
        let (tokens, ld) = lex("import { fib, add } from \"./math\";");
        assert!(ld.is_empty(), "{ld:?}");
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        let Item::Import(imp) = &ast[0] else { panic!("expected import") };
        let names: Vec<&str> = imp.names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["fib", "add"]);
        assert_eq!(imp.path, "./math");
    }

    #[test]
    fn export_marks_functions_and_structs() {
        let (tokens, _) = lex(
            "export fun f(): int { return 1; } export struct P { x: int } fun g() { }",
        );
        let (ast, pd) = parse(&tokens);
        assert!(pd.is_empty(), "parse errors: {pd:?}");
        let Item::Function(f) = &ast[0] else { panic!() };
        assert!(f.exported);
        let Item::Struct(s) = &ast[1] else { panic!() };
        assert!(s.exported);
        let Item::Function(g) = &ast[2] else { panic!() };
        assert!(!g.exported);
    }

    #[test]
    fn export_before_junk_is_reported_and_recovered() {
        let (tokens, _) = lex("export 42 fun ok(): int { return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1, "{pd:?}");
        assert!(pd[0].message.contains("after 'export'"), "{pd:?}");
        assert_eq!(ast.len(), 1);
    }

    #[test]
    fn import_with_missing_path_recovers() {
        let (tokens, _) = lex("import { f } from ; fun ok(): int { return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1, "{pd:?}");
        assert!(pd[0].message.contains("module path"), "{pd:?}");
        assert_eq!(ast.len(), 2); // the (broken) import + the function
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

    #[test]
    fn equality_binds_looser_than_comparison_tighter_than_logic() {
        assert_eq!(
            expr("a == b && c < d").sexpr(),
            "(&& (== a b) (< c d))"
        );
        assert_eq!(expr("a < b == c >= d").sexpr(), "(== (< a b) (>= c d))");
    }

    #[test]
    fn bool_and_string_atoms() {
        assert_eq!(expr("true").sexpr(), "true");
        assert_eq!(expr("!false").sexpr(), "(! false)");
        assert_eq!(expr("\"hi\" + \"!\"").sexpr(), "(+ \"hi\" \"!\")");
    }

    #[test]
    fn if_with_else_if_chain() {
        let s = stmt("if a { return 1; } else if b { return 2; } else { return 3; }");
        let Stmt::If { cond, then_body, else_body, .. } = s else {
            panic!("expected If, got something else");
        };
        assert_eq!(cond.sexpr(), "a");
        assert_eq!(then_body.len(), 1);
        // The `else if` is a single nested If in the else body.
        let else_body = else_body.expect("else body");
        assert_eq!(else_body.len(), 1);
        let Stmt::If { cond: nested_cond, else_body: nested_else, .. } = &else_body[0] else {
            panic!("expected nested If in else body");
        };
        assert_eq!(nested_cond.sexpr(), "b");
        assert!(nested_else.is_some());
    }

    #[test]
    fn while_statement() {
        let s = stmt("while i < n { i = i + 1; }");
        let Stmt::While { cond, body, .. } = s else {
            panic!("expected While");
        };
        assert_eq!(cond.sexpr(), "(< i n)");
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn condition_is_never_a_struct_literal() {
        // `if x { … }` must read `x` as an identifier condition, not the
        // struct literal `x { }`.
        let s = stmt("if x { return 1; }");
        let Stmt::If { cond, .. } = s else { panic!("expected If") };
        assert_eq!(cond.sexpr(), "x");
    }

    #[test]
    fn parenthesized_condition_allows_struct_literals_again() {
        let s = stmt("if (P { x: 1 }).x == 1 { return 1; }");
        let Stmt::If { cond, .. } = s else { panic!("expected If") };
        assert_eq!(cond.sexpr(), "(== (. (struct P x=1) x) 1)");
    }

    #[test]
    fn nested_block_error_does_not_eat_the_following_statement() {
        // Regression: the outer block used to see the nested block's (already
        // recovered) diagnostic and synchronize again, swallowing `x = 5;`.
        let (tokens, _) =
            lex("fun f(): int { var x = 0; if true { const = 1; } x = 5; return x; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1, "{pd:?}");
        let Item::Function(f) = &ast[0] else { panic!("expected function") };
        assert_eq!(f.body.len(), 4, "x = 5; must survive: {:?}", f.body);
        assert!(matches!(f.body[2], Stmt::Assign { .. }));
    }

    #[test]
    fn struct_literal_allowed_in_call_arguments_inside_condition() {
        let s = stmt("if eq(P { x: 1 }) { return 1; }");
        let Stmt::If { cond, .. } = s else { panic!("expected If") };
        assert_eq!(cond.sexpr(), "(call eq (struct P x=1))");
    }

    #[test]
    fn missing_semicolon_after_struct_literal_does_not_cascade() {
        // Regression: the statement ends in the struct literal's `}`, which
        // used to spoof the "ended cleanly" check and skip recovery.
        let (tokens, _) = lex("fun f(): int { const p = P { x: 1 } ) return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 1, "one missing-semicolon error only: {pd:?}");
        let Item::Function(f) = &ast[0] else { panic!("expected function") };
        assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
    }

    #[test]
    fn error_placeholder_consuming_a_semicolon_does_not_cascade() {
        // Regression: `var x = ;` — parse_atom's recovery consumed the `;`,
        // which used to spoof the clean-end check; the junk after it then
        // parsed as phantom statements.
        let (tokens, _) = lex("fun f(): int { var x = ; y z w return 1; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 2, "missing expression + missing semicolon: {pd:?}");
        let Item::Function(f) = &ast[0] else { panic!("expected function") };
        assert_eq!(f.body.len(), 2, "no phantom statements: {:?}", f.body);
        assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
    }

    #[test]
    fn pathological_nesting_errors_instead_of_overflowing_the_stack() {
        // 500 nested parens.
        let src = format!(
            "fun f(): int {{ return {}1{}; }}",
            "(".repeat(500),
            ")".repeat(500)
        );
        let (tokens, _) = lex(&src);
        let (_, pd) = parse(&tokens);
        assert!(
            pd.iter().any(|e| e.message.contains("nesting exceeds")),
            "expected a nesting diagnostic: {} diags",
            pd.len()
        );

        // A 500-deep `else if` chain.
        let mut src = String::from("fun f(a: bool): int { if a { return 1; }");
        for _ in 0..500 {
            src.push_str(" else if a { return 1; }");
        }
        src.push_str(" else { return 0; } }");
        let (tokens, _) = lex(&src);
        let (_, pd) = parse(&tokens);
        assert!(
            pd.iter().any(|e| e.message.contains("nesting exceeds")),
            "expected a nesting diagnostic: {} diags",
            pd.len()
        );
    }

    #[test]
    fn malformed_statement_recovers_at_statement_boundary() {
        // `const = 1 2 3;` is broken; recovery skips to the `;` so the
        // following statement parses cleanly instead of cascading.
        let (tokens, _) = lex("fun f(): int { const = 1 2 3; return 7; }");
        let (ast, pd) = parse(&tokens);
        assert_eq!(pd.len(), 2, "one missing-name error + one at the `2`: {pd:?}");
        let Item::Function(f) = &ast[0] else { panic!("expected function") };
        assert!(matches!(f.body.last(), Some(Stmt::Return { .. })));
    }
}
