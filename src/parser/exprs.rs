//! Expressions: a Pratt parser. `parse_expr` loops binding powers,
//! `parse_prefix`/`parse_atom` handle leaves and unary operators,
//! `parse_postfix` the `.field`/`[index]`/`(args)` chains. Operator
//! chains charge the per-function budget (`claim_op`) because they grow
//! the AST without recursing.

use super::*;

impl Parser<'_> {
    pub fn parse_expr(&mut self, min_bp: u8) -> Expr {
        if !self.enter_nested() {
            return Expr::Int(0, self.peek().span); // recovery placeholder
        }
        let expr = self.parse_expr_inner(min_bp);
        self.depth -= 1;
        expr
    }

    pub(super) fn parse_expr_inner(&mut self, min_bp: u8) -> Expr {
        let mut lhs = self.parse_prefix();
        loop {
            // Postfix (call `(`, field `.`/`?.`) binds tighter than every operator.
            if matches!(
                self.peek().kind,
                TokenKind::LeftParen
                    | TokenKind::LeftBracket
                    | TokenKind::Dot
                    | TokenKind::QuestionDot
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
            let op_span = self.peek().span;
            self.bump(); // operator
            let rhs = self.parse_expr(prec.right_bp());
            if self.claim_op(op_span) {
                let span = lhs.span().to(rhs.span());
                lhs = Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                };
            }
        }
        lhs
    }

    pub(super) fn parse_prefix(&mut self) -> Expr {
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

    /// A template literal (ADR 0030) desugars during parsing: text runs
    /// become string literals, each `${e}` an implicit `string(e)`, all
    /// folded left over `+` concat — the checker and both engines only
    /// ever see the desugar. Spans stay unique in the span-keyed type
    /// table: a conversion spans its `${...}` delimiters, text keeps its
    /// token span, each fold node runs from the backtick to its
    /// rightmost part.
    fn parse_template(&mut self, first: String, head_span: Span) -> Expr {
        let mut acc = (!first.is_empty()).then_some(Expr::Str(first, head_span));
        let mut delim_end = head_span.end; // just past this part's `${`
        loop {
            let prev = self.struct_literals_allowed;
            self.struct_literals_allowed = true;
            let arg = self.parse_expr(0);
            self.struct_literals_allowed = prev;
            let next = self.advance(); // `}text${`, `}text\``, or junk
            let conv = Expr::Convert {
                to: Conv::Str,
                implicit: true,
                arg: Box::new(arg),
                span: Span::new(delim_end - 2, next.span.start + 1),
            };
            acc = Some(self.template_join(acc, conv, head_span));
            match next.kind {
                TokenKind::TemplateMiddle(text) => {
                    if !text.is_empty() {
                        let part = Expr::Str(text, next.span);
                        acc = Some(self.template_join(acc, part, head_span));
                    }
                    delim_end = next.span.end;
                }
                TokenKind::TemplateTail(text) => {
                    if !text.is_empty() {
                        let part = Expr::Str(text, next.span);
                        acc = Some(self.template_join(acc, part, head_span));
                    }
                    return acc.expect("at least one interpolation part");
                }
                other => {
                    self.error(
                        format!(
                            "expected '}}' to close the interpolation, found {}",
                            describe(&other)
                        ),
                        next.span,
                    );
                    return acc.expect("at least one interpolation part");
                }
            }
        }
    }

    /// One fold step of the template desugar: `acc + part`.
    fn template_join(&mut self, acc: Option<Expr>, part: Expr, start: Span) -> Expr {
        let Some(lhs) = acc else { return part };
        if !self.claim_op(start) {
            return lhs; // budget exceeded: freeze growth, like chains
        }
        let span = start.to(part.span());
        Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(lhs),
            rhs: Box::new(part),
            span,
        }
    }

    pub(super) fn parse_atom(&mut self) -> Expr {
        let tok = self.advance();
        match tok.kind {
            TokenKind::IntLiteral(n) => Expr::Int(n, tok.span),
            TokenKind::FloatLiteral(f) => Expr::Float(f, tok.span),
            TokenKind::True => Expr::Bool(true, tok.span),
            TokenKind::False => Expr::Bool(false, tok.span),
            TokenKind::Null => Expr::Null(tok.span),
            // `error.Name` — an error-code literal (ADR 0034). Bare
            // `error` in expression position arrives with `T!` tests.
            TokenKind::ErrorKw => {
                if self.eat(&TokenKind::Dot) {
                    let name_span = self.peek().span;
                    let name = self.expect_identifier();
                    Expr::ErrorLit(name, tok.span.to(name_span))
                } else {
                    self.error(
                        "expected '.' and an error name after 'error'".to_string(),
                        tok.span,
                    );
                    Expr::Null(tok.span) // recovery: already reported
                }
            }
            TokenKind::StringLiteral(s) => Expr::Str(s, tok.span),
            TokenKind::Identifier(name) => {
                if self.struct_literals_allowed && self.check(&TokenKind::LeftBrace) {
                    self.parse_struct_literal(name, tok.span)
                } else {
                    Expr::Ident(name, tok.span)
                }
            }
            // `int(x)` / `float(x)` / `string(x)` — conversion calls
            // (ADR 0028/0029); the type keywords cannot be shadowed, so
            // the form is unambiguous.
            kind @ (TokenKind::IntType | TokenKind::FloatType | TokenKind::StringType) => {
                self.expect(TokenKind::LeftParen);
                let arg = self.parse_expr(0);
                let end = self.expect(TokenKind::RightParen);
                Expr::Convert {
                    to: match kind {
                        TokenKind::IntType => Conv::Int,
                        TokenKind::FloatType => Conv::Float,
                        _ => Conv::Str,
                    },
                    implicit: false,
                    arg: Box::new(arg),
                    span: tok.span.to(end),
                }
            }
            TokenKind::TemplateHead(first) => self.parse_template(first, tok.span),
            TokenKind::LeftParen => {
                // Parentheses re-enable struct literals inside a condition.
                let prev = self.struct_literals_allowed;
                self.struct_literals_allowed = true;
                let inner = self.parse_expr(0);
                self.struct_literals_allowed = prev;
                self.expect(TokenKind::RightParen);
                inner
            }
            TokenKind::LeftBracket => {
                // Array literal brackets re-enable struct literals, same as
                // grouping parentheses.
                let prev = self.struct_literals_allowed;
                self.struct_literals_allowed = true;
                let mut elements = Vec::new();
                while !self.check(&TokenKind::RightBracket) && !self.at_eof() {
                    elements.push(self.parse_expr(0));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.struct_literals_allowed = prev;
                let end = self.expect(TokenKind::RightBracket);
                Expr::ArrayLit {
                    elements,
                    span: tok.span.to(end),
                }
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

    pub(super) fn parse_postfix(&mut self, lhs: Expr) -> Expr {
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
                if !self.claim_op(end) {
                    return lhs;
                }
                let span = lhs.span().to(end);
                Expr::Call {
                    callee: Box::new(lhs),
                    args,
                    span,
                }
            }
            TokenKind::LeftBracket => {
                self.bump();
                // Index brackets re-enable struct literals inside a
                // condition, same as call parentheses.
                let prev = self.struct_literals_allowed;
                self.struct_literals_allowed = true;
                let index = self.parse_expr(0);
                self.struct_literals_allowed = prev;
                let end = self.expect(TokenKind::RightBracket);
                if !self.claim_op(end) {
                    return lhs;
                }
                let span = lhs.span().to(end);
                Expr::Index {
                    base: Box::new(lhs),
                    index: Box::new(index),
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
                if !self.claim_op(name_span) {
                    return lhs;
                }
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

    pub(super) fn parse_struct_literal(&mut self, name: String, start: Span) -> Expr {
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
}
