//! Top-level declarations and type annotations: `fun`, `struct`,
//! `refstruct`, `import`, and the `T?`/`T[]` annotation grammar —
//! plus item-boundary error recovery (`synchronize`).

use super::*;

impl Parser<'_> {
    /// A base type with postfix suffixes: `?` (optional) and `[]` (array),
    /// composable left to right — `int?[]` is an array of optional ints,
    /// `int[]?` an optional array. Doubling the optional is rejected with a
    /// dedicated message (the lexer reads `??` greedily as one token).
    pub(super) fn parse_type(&mut self) -> TypeAnn {
        let mut ty = self.parse_base_type();
        loop {
            match self.peek().kind {
                TokenKind::Question | TokenKind::QuestionQuestion => {
                    let mut doubled = self.peek().kind == TokenKind::QuestionQuestion
                        || matches!(ty, TypeAnn::Optional(_));
                    let span = self.peek().span;
                    self.bump();
                    // Swallow any further question tokens — however many
                    // the user typed, one mistake reports once.
                    while matches!(
                        self.peek().kind,
                        TokenKind::Question | TokenKind::QuestionQuestion
                    ) {
                        doubled = true;
                        self.bump();
                    }
                    if doubled {
                        self.error(
                            "nested optionals are not allowed — 'T??' is just 'T?'".to_string(),
                            span,
                        );
                    }
                    if !matches!(ty, TypeAnn::Optional(_)) {
                        ty = TypeAnn::Optional(Box::new(ty));
                    }
                }
                TokenKind::LeftBracket => {
                    self.bump();
                    self.expect(TokenKind::RightBracket);
                    ty = TypeAnn::Array(Box::new(ty));
                }
                _ => return ty,
            }
        }
    }

    pub(super) fn parse_base_type(&mut self) -> TypeAnn {
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

    pub(super) fn parse_function(&mut self, exported: bool) -> Function {
        self.fn_ops = 0;
        self.fn_ops_reported = false;
        let start = self.expect(TokenKind::Fun);
        let name = self.expect_identifier();
        self.expect(TokenKind::LeftParen);
        let mut params = Vec::new();
        while !self.check(&TokenKind::RightParen) && !self.at_eof() {
            let param_name = self.expect_identifier();
            self.expect(TokenKind::Colon);
            let ty = self.parse_type();
            params.push(Param {
                name: param_name,
                ty,
            });
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

    pub(super) fn parse_struct(&mut self, exported: bool) -> Struct {
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
            fields.push(Field {
                name: field_name,
                ty,
            });
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
    pub(super) fn synchronize(&mut self) {
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
    /// power the current expression must exceed to keep absorbing operators.    /// Parses `import { a, b } from "./path";`.
    pub(super) fn parse_import(&mut self) -> ImportDecl {
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
                format!(
                    "expected a module path string, found {}",
                    describe(&tok.kind)
                ),
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
}
