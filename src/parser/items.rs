//! Top-level declarations and type annotations: `fun`, `struct`,
//! `refstruct`, `import`, and the `T?`/`T[]` annotation grammar —
//! plus item-boundary error recovery (`synchronize`).

use super::*;

impl Parser {
    /// A base type with postfix suffixes: `?` (optional), `!` (error
    /// union, ADR 0034), and `[]` (array), composable left to right —
    /// `int?[]` is an array of optional ints, `int[]?` an optional
    /// array, `int[]!` an array-or-error. Doubling the optional is
    /// rejected with a dedicated message (the lexer reads `??` greedily
    /// as one token), and optionals and error unions do not mix
    /// (`T?!`/`T!?` — the ADR 0034 reserved seat).
    pub(super) fn parse_type(&mut self) -> TypeAnn {
        let mut ty = self.parse_base_type();
        loop {
            match self.peek().kind {
                TokenKind::Bang => {
                    let span = self.peek().span;
                    self.bump();
                    match &ty {
                        TypeAnn::ErrUnion(_) => {
                            self.error(
                                "nested error unions are not allowed — 'T!!' is just 'T!'"
                                    .to_string(),
                                span,
                            );
                        }
                        TypeAnn::Optional(_) => {
                            self.error(
                                "optionals and error unions do not mix — 'T?!' is not yet supported"
                                    .to_string(),
                                span,
                            );
                        }
                        _ => ty = TypeAnn::ErrUnion(Box::new(ty)),
                    }
                }
                TokenKind::Question | TokenKind::QuestionQuestion => {
                    if matches!(ty, TypeAnn::ErrUnion(_)) {
                        let span = self.peek().span;
                        self.bump();
                        self.error(
                            "optionals and error unions do not mix — 'T!?' is not yet supported"
                                .to_string(),
                            span,
                        );
                        continue;
                    }
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
            TokenKind::FileType => {
                self.bump();
                TypeAnn::File
            }
            TokenKind::ErrorKw => {
                self.bump();
                TypeAnn::ErrCode
            }
            TokenKind::Identifier(n) => {
                self.bump();
                if self.check(&TokenKind::Less) {
                    self.parse_applied_type(n)
                } else {
                    TypeAnn::Named(n)
                }
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

    /// `Pair<int, string>` — a generic type application (ADR 0035). In
    /// type position `<` after a name is unambiguous. Applied types are
    /// the type grammar's only recursion point, so they claim nesting.
    fn parse_applied_type(&mut self, name: String) -> TypeAnn {
        if !self.enter_nested() {
            return TypeAnn::Named(name); // recovery placeholder
        }
        self.bump(); // '<'
        let mut args = Vec::new();
        loop {
            args.push(self.parse_type());
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.close_type_args();
        self.depth -= 1;
        TypeAnn::Applied(name, args)
    }

    /// Consumes the `>` closing a type-argument list. A `>=` token is
    /// split in place — `var b: Box<int>= x` — by rewriting it to the
    /// remaining `=` (ADR 0035); ys has no shift operator, so `>>`
    /// already lexes as two tokens.
    pub(super) fn close_type_args(&mut self) {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Greater => self.bump(),
            TokenKind::GreaterEq => {
                self.tokens[self.pos] = Token {
                    kind: TokenKind::Equals,
                    span: Span::new(tok.span.start + 1, tok.span.end),
                };
            }
            other => {
                self.error(
                    format!(
                        "expected '>' to close the type arguments, found {}",
                        describe(&other)
                    ),
                    tok.span,
                );
            }
        }
    }

    /// `<T, U>` after a function or struct name — generic type
    /// parameters (ADR 0035). Absent means an ordinary declaration.
    fn parse_type_params(&mut self) -> Vec<(String, Span)> {
        if !self.eat(&TokenKind::Less) {
            return Vec::new();
        }
        let mut params = Vec::new();
        loop {
            let span = self.peek().span;
            let name = self.expect_identifier();
            params.push((name, span));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Greater);
        params
    }

    pub(super) fn parse_function(&mut self, exported: bool) -> Function {
        self.fn_ops = 0;
        self.fn_ops_reported = false;
        let start = self.expect(TokenKind::Fun);
        let name = self.expect_identifier();
        let type_params = self.parse_type_params();
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
            type_params,
            params,
            return_type,
            body,
            span: start.to(end),
        }
    }

    /// Parses `enum Name<T, U> { Variant(T), Ready }` (ADR 0036) —
    /// comma-separated variants, each with optional positional payload
    /// types. The caller dispatched on the `enum` keyword.
    pub(super) fn parse_enum(&mut self, exported: bool) -> EnumDecl {
        let start = self.expect(TokenKind::Enum);
        let name = self.expect_identifier();
        let type_params = self.parse_type_params();
        self.expect(TokenKind::LeftBrace);
        let mut variants = Vec::new();
        while !self.check(&TokenKind::RightBrace) && !self.at_eof() {
            let vspan = self.peek().span;
            let vname = self.expect_identifier();
            let mut payloads = Vec::new();
            if self.eat(&TokenKind::LeftParen) {
                while !self.check(&TokenKind::RightParen) && !self.at_eof() {
                    payloads.push(self.parse_type());
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RightParen);
            }
            variants.push(Variant {
                name: vname,
                payloads,
                span: vspan,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect(TokenKind::RightBrace);
        EnumDecl {
            exported,
            name,
            type_params,
            variants,
            span: start.to(end),
        }
    }

    /// Parses `error Name[, Name]*;` — module-scoped error codes
    /// (ADR 0034). The caller dispatched on the `error` keyword.
    pub(super) fn parse_error_decl(&mut self, exported: bool) -> ErrorDecl {
        let start = self.expect(TokenKind::ErrorKw);
        let mut names = Vec::new();
        loop {
            let span = self.peek().span;
            let name = self.expect_identifier();
            names.push((name, span));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect(TokenKind::Semicolon);
        ErrorDecl {
            exported,
            names,
            span: start.to(end),
        }
    }

    pub(super) fn parse_struct(&mut self, exported: bool) -> Struct {
        // The caller dispatched on the keyword — `struct` or `refstruct`.
        let kw = self.advance();
        let by_ref = kw.kind == TokenKind::RefStruct;
        let start = kw.span;
        let name = self.expect_identifier();
        let type_params = self.parse_type_params();
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
            type_params,
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
                | TokenKind::Enum
                | TokenKind::Import
                | TokenKind::Export
                | TokenKind::ErrorKw => return,
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
