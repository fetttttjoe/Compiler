use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::syntax;
use crate::token::{Token, TokenKind};

/// Tokenizes `source`. Always returns a stream ending in `TokenKind::Eof`.
/// Unknown characters produce a diagnostic and are skipped (recovery).
pub fn lex(source: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer {
        source,
        pos: 0,
        diagnostics: Vec::new(),
    };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let is_eof = matches!(token.kind, TokenKind::Eof);
        tokens.push(token);
        if is_eof {
            break;
        }
    }
    (tokens, lexer.diagnostics)
}

struct Lexer<'a> {
    source: &'a str,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if syntax::is_whitespace(c)) {
            self.bump();
        }
    }

    fn next_token(&mut self) -> Token {
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(c) = self.peek() else {
                return Token {
                    kind: TokenKind::Eof,
                    span: Span::new(start, start),
                };
            };
            // `None` means trivia was consumed (comment / unknown char) — retry.
            if let Some(kind) = self.scan(c) {
                return Token {
                    kind,
                    span: Span::new(start, self.pos),
                };
            }
        }
    }

    fn scan(&mut self, c: char) -> Option<TokenKind> {
        match c {
            syntax::LPAREN => self.single(TokenKind::LeftParen),
            syntax::RPAREN => self.single(TokenKind::RightParen),
            syntax::LBRACE => self.single(TokenKind::LeftBrace),
            syntax::RBRACE => self.single(TokenKind::RightBrace),
            syntax::COLON => self.single(TokenKind::Colon),
            syntax::SEMICOLON => self.single(TokenKind::Semicolon),
            syntax::COMMA => self.single(TokenKind::Comma),
            syntax::DOT => self.single(TokenKind::Dot),
            syntax::EQUALS => self.single(TokenKind::Equals),
            syntax::PLUS => self.single(TokenKind::Plus),
            syntax::MINUS => self.single(TokenKind::Minus),
            syntax::STAR => self.single(TokenKind::Asterisk),
            syntax::PERCENT => self.single(TokenKind::Percent),
            syntax::BANG => self.single(TokenKind::Bang),
            syntax::LESS => self.single(TokenKind::Less),
            syntax::GREATER => self.single(TokenKind::Greater),
            syntax::AMPERSAND => self.double(syntax::AMPERSAND, TokenKind::AmpAmp),
            syntax::PIPE => self.double(syntax::PIPE, TokenKind::PipePipe),
            syntax::SLASH => self.scan_slash_or_comment(),
            c if c.is_ascii_digit() => self.scan_number(),
            c if c.is_alphabetic() || c == syntax::UNDERSCORE => Some(self.scan_identifier()),
            other => {
                let start = self.pos;
                self.bump();
                self.diagnostics.push(Diagnostic::error(
                    format!("unexpected character '{other}'"),
                    Span::new(start, self.pos),
                ));
                None
            }
        }
    }

    fn single(&mut self, kind: TokenKind) -> Option<TokenKind> {
        self.bump();
        Some(kind)
    }

    /// `first` is the current char and must be doubled (e.g. `&&`, `||`).
    fn double(&mut self, first: char, both: TokenKind) -> Option<TokenKind> {
        let start = self.pos;
        self.bump();
        if self.peek() == Some(first) {
            self.bump();
            Some(both)
        } else {
            self.diagnostics.push(Diagnostic::error(
                format!("expected '{first}{first}'"),
                Span::new(start, self.pos),
            ));
            None
        }
    }

    /// A `/` is either a `//` line comment (to end of line, on any platform) or
    /// the division operator.
    fn scan_slash_or_comment(&mut self) -> Option<TokenKind> {
        self.bump(); // first '/'
        if self.peek() == Some(syntax::SLASH) {
            while let Some(c) = self.peek() {
                if syntax::is_line_break(c) {
                    break;
                }
                self.bump();
            }
            None
        } else {
            Some(TokenKind::Slash)
        }
    }

    fn scan_number(&mut self) -> Option<TokenKind> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        let mut is_float = false;
        if self.peek() == Some(syntax::DOT) {
            let after_dot = self.source[self.pos + 1..].chars().next();
            if matches!(after_dot, Some(c) if c.is_ascii_digit()) {
                is_float = true;
                self.bump(); // '.'
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.bump();
                }
            }
        }
        let text = &self.source[start..self.pos];
        if is_float {
            match text.parse::<f64>() {
                Ok(f) => Some(TokenKind::FloatLiteral(f)),
                Err(_) => {
                    self.diagnostics.push(Diagnostic::error(
                        format!("invalid float literal '{text}'"),
                        Span::new(start, self.pos),
                    ));
                    None
                }
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => Some(TokenKind::IntLiteral(n)),
                Err(_) => {
                    self.diagnostics.push(Diagnostic::error(
                        format!("integer literal '{text}' out of range"),
                        Span::new(start, self.pos),
                    ));
                    Some(TokenKind::IntLiteral(0)) // recover with a placeholder value
                }
            }
        }
    }

    fn scan_identifier(&mut self) -> TokenKind {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == syntax::UNDERSCORE) {
            self.bump();
        }
        match &self.source[start..self.pos] {
            syntax::KW_FUN => TokenKind::Fun,
            syntax::KW_STRUCT => TokenKind::Struct,
            syntax::KW_VAR => TokenKind::Var,
            syntax::KW_CONST => TokenKind::Const,
            syntax::KW_RETURN => TokenKind::Return,
            syntax::KW_INT => TokenKind::IntType,
            syntax::KW_FLOAT => TokenKind::FloatType,
            other => TokenKind::Identifier(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).0.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn keywords_and_identifiers() {
        assert_eq!(
            kinds("fun foo"),
            vec![TokenKind::Fun, TokenKind::Identifier("foo".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn integer_and_float_literals() {
        assert_eq!(
            kinds("1 2.5"),
            vec![TokenKind::IntLiteral(1), TokenKind::FloatLiteral(2.5), TokenKind::Eof]
        );
    }

    #[test]
    fn comment_runs_to_end_of_line_on_any_platform() {
        // LF, CRLF, and lone-CR terminated comments each stop before the next line.
        for src in ["fun // c\nx", "fun // c\r\nx", "fun // c\rx"] {
            assert_eq!(
                kinds(src),
                vec![TokenKind::Fun, TokenKind::Identifier("x".into()), TokenKind::Eof],
                "failed for {src:?}"
            );
        }
    }

    #[test]
    fn tabs_and_newlines_are_whitespace() {
        assert_eq!(
            kinds("\tfun\r\nx"),
            vec![TokenKind::Fun, TokenKind::Identifier("x".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn double_operators() {
        assert_eq!(
            kinds("&& ||"),
            vec![TokenKind::AmpAmp, TokenKind::PipePipe, TokenKind::Eof]
        );
    }

    #[test]
    fn unknown_character_reports_a_diagnostic_and_recovers() {
        let (tokens, diags) = lex("a # b");
        assert_eq!(
            tokens.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
            vec![
                TokenKind::Identifier("a".into()),
                TokenKind::Identifier("b".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("unexpected character '#'"));
    }

    #[test]
    fn spans_cover_the_token_text() {
        let (tokens, _) = lex("fun");
        assert_eq!(tokens[0].span, Span::new(0, 3));
    }
}
