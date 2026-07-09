//! Statements and blocks: bindings, assignment, control flow, and
//! statement-level error recovery (skip to the next statement boundary,
//! never cascade).

use super::*;

impl Parser<'_> {
    /// Parses an `if`/`while` condition: struct literals are disallowed so
    /// `if x { … }` reads `x` as the condition, not a struct literal `x {}`.
    pub(super) fn parse_condition(&mut self) -> Expr {
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
    pub(super) fn parse_block(&mut self) -> (Vec<Stmt>, Span, bool) {
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
    pub(super) fn synchronize_stmt(&mut self) {
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
                | TokenKind::While
                | TokenKind::For => return,
                _ => self.bump(),
            }
        }
    }

    pub(super) fn parse_if(&mut self) -> (Stmt, bool) {
        // `else if` chains recurse here directly, so they claim depth too.
        if !self.enter_nested() {
            return (Stmt::Expr(Expr::Int(0, self.peek().span)), false); // recovery placeholder
        }
        let result = self.parse_if_inner();
        self.depth -= 1;
        result
    }

    pub(super) fn parse_if_inner(&mut self) -> (Stmt, bool) {
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
                if let Stmt::If {
                    span: nested_span, ..
                } = &nested
                {
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

    pub(super) fn parse_stmt_inner(&mut self) -> (Stmt, bool) {
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
            TokenKind::For => {
                self.bump();
                // `for x in …` or `for [i, x] in …`. One malformed header
                // token = one diagnostic: the first failed expect bails to
                // statement recovery instead of dragging every following
                // expect over the same token.
                let header = (|| {
                    let (index, name) = if self.eat(&TokenKind::LeftBracket) {
                        let index = self.header_identifier()?;
                        self.header_token(TokenKind::Comma)?;
                        let name = self.header_identifier()?;
                        self.header_token(TokenKind::RightBracket)?;
                        (Some(index), name)
                    } else {
                        (None, self.header_identifier()?)
                    };
                    self.header_token(TokenKind::In)?;
                    Some((index, name))
                })();
                let Some((index, name)) = header else {
                    // Skip the rest of the header and swallow the loop body
                    // so the brace structure stays balanced.
                    while !self.at_eof()
                        && !matches!(
                            self.peek().kind,
                            TokenKind::LeftBrace | TokenKind::Semicolon | TokenKind::RightBrace
                        )
                    {
                        self.bump();
                    }
                    if self.check(&TokenKind::LeftBrace) {
                        let _ = self.parse_block();
                        return (Stmt::Expr(Expr::Int(0, tok.span)), true);
                    }
                    return (Stmt::Expr(Expr::Int(0, tok.span)), false);
                };
                // Struct literals are off in the iterable, same as
                // conditions: `for x in xs { … }` must read `xs` then a
                // block, not a struct literal `xs { … }`.
                let iterable = self.parse_condition();
                let (body, end, clean) = self.parse_block();
                (
                    Stmt::For {
                        index,
                        name,
                        iterable,
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
                    if !expr.is_place() {
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
