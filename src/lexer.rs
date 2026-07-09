//! Source text → tokens. One linear scan with one character of
//! lookahead; every token carries its span. Escapes are decoded here
//! (bad ones recover with the raw character), comments and whitespace
//! vanish, and the stream always ends with an `Eof` sentinel the parser
//! relies on.

use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::syntax;
use crate::token::{Token, TokenKind};

/// Tokenizes a standalone source string (base offset 0) — the single-file
/// convenience used throughout the test suites; production code lexes files
/// into the global offset space via `lex_at`.
#[cfg(test)]
pub fn lex(source: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    lex_at(source, 0)
}

/// Like `lex`, but every span is offset by `base` — the file's position in
/// the `SourceMap`'s global offset space.
pub fn lex_at(source: &str, base: usize) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lexer = Lexer {
        source,
        pos: 0,
        base,
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
    base: usize,
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

    /// A span from a local `start` to the current position, in global
    /// (base-offset) coordinates.
    fn abs_span(&self, start: usize) -> Span {
        Span::new(self.base + start, self.base + self.pos)
    }

    fn error(&mut self, message: String, span: Span) {
        self.diagnostics.push(Diagnostic::error(message, span));
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
                    span: self.abs_span(start),
                };
            };
            // `None` means trivia was consumed (comment / unknown char) — retry.
            if let Some(kind) = self.scan(c) {
                return Token {
                    kind,
                    span: self.abs_span(start),
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
            syntax::LBRACKET => self.single(TokenKind::LeftBracket),
            syntax::RBRACKET => self.single(TokenKind::RightBracket),
            syntax::COLON => self.single(TokenKind::Colon),
            syntax::SEMICOLON => self.single(TokenKind::Semicolon),
            syntax::COMMA => self.single(TokenKind::Comma),
            syntax::DOT => self.single(TokenKind::Dot),
            syntax::EQUALS => self.maybe_eq(TokenKind::Equals, TokenKind::EqEq),
            syntax::PLUS => self.single(TokenKind::Plus),
            syntax::MINUS => self.single(TokenKind::Minus),
            syntax::STAR => self.single(TokenKind::Asterisk),
            syntax::PERCENT => self.single(TokenKind::Percent),
            syntax::BANG => self.maybe_eq(TokenKind::Bang, TokenKind::BangEq),
            syntax::LESS => self.maybe_eq(TokenKind::Less, TokenKind::LessEq),
            syntax::GREATER => self.maybe_eq(TokenKind::Greater, TokenKind::GreaterEq),
            syntax::QUOTE => self.scan_string(),
            syntax::AMPERSAND => self.double(syntax::AMPERSAND, TokenKind::AmpAmp),
            syntax::PIPE => self.double(syntax::PIPE, TokenKind::PipePipe),
            syntax::QUESTION => self.scan_question(),
            syntax::SLASH => self.scan_slash_or_comment(),
            c if c.is_ascii_digit() => self.scan_number(),
            c if c.is_alphabetic() || c == syntax::UNDERSCORE => Some(self.scan_identifier()),
            other => {
                let start = self.pos;
                self.bump();
                self.error(
                    format!("unexpected character '{other}'"),
                    self.abs_span(start),
                );
                None
            }
        }
    }

    fn single(&mut self, kind: TokenKind) -> Option<TokenKind> {
        self.bump();
        Some(kind)
    }

    /// Consumes one char; a following `=` upgrades `single` to `double`
    /// (`=`→`==`, `!`→`!=`, `<`→`<=`, `>`→`>=`).
    fn maybe_eq(&mut self, single: TokenKind, double: TokenKind) -> Option<TokenKind> {
        self.bump();
        if self.peek() == Some(syntax::EQUALS) {
            self.bump();
            Some(double)
        } else {
            Some(single)
        }
    }

    /// `?` and its two-char forms: `??` (coalescing) and `?.` (chaining).
    fn scan_question(&mut self) -> Option<TokenKind> {
        self.bump();
        match self.peek() {
            Some(syntax::QUESTION) => {
                self.bump();
                Some(TokenKind::QuestionQuestion)
            }
            Some(syntax::DOT) => {
                self.bump();
                Some(TokenKind::QuestionDot)
            }
            _ => Some(TokenKind::Question),
        }
    }

    /// A double-quoted, single-line string literal with `\"`, `\\`, `\n`, `\t`
    /// escapes. Unterminated or unknown-escape input produces a diagnostic.
    fn scan_string(&mut self) -> Option<TokenKind> {
        let start = self.pos;
        self.bump(); // opening quote
        let mut text = String::new();
        loop {
            match self.peek() {
                None => {
                    self.error(
                        "unterminated string literal".to_string(),
                        self.abs_span(start),
                    );
                    return None;
                }
                Some(c) if syntax::is_line_break(c) => {
                    self.error(
                        "unterminated string literal".to_string(),
                        self.abs_span(start),
                    );
                    return None;
                }
                Some(syntax::QUOTE) => {
                    self.bump();
                    return Some(TokenKind::StringLiteral(text));
                }
                Some(syntax::BACKSLASH) => {
                    let escape_start = self.pos;
                    self.bump();
                    match self.peek() {
                        Some(syntax::QUOTE) => text.push(syntax::QUOTE),
                        Some(syntax::BACKSLASH) => text.push(syntax::BACKSLASH),
                        Some(syntax::ESCAPE_LF) => text.push(syntax::LF),
                        Some(syntax::ESCAPE_TAB) => text.push(syntax::TAB),
                        // A real line break right after `\` ends the line —
                        // the string is unterminated, same as the outer loop.
                        Some(c) if syntax::is_line_break(c) => {
                            self.error(
                                "unterminated string literal".to_string(),
                                self.abs_span(start),
                            );
                            return None;
                        }
                        Some(other) => {
                            self.error(
                                format!("unknown escape '\\{other}'"),
                                Span::new(
                                    self.base + escape_start,
                                    self.base + self.pos + other.len_utf8(),
                                ),
                            );
                            text.push(other); // recover with the raw character
                        }
                        None => continue, // EOF: the loop reports unterminated
                    }
                    self.bump();
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
    }

    /// `first` is the current char and must be doubled (e.g. `&&`, `||`).
    fn double(&mut self, first: char, both: TokenKind) -> Option<TokenKind> {
        let start = self.pos;
        self.bump();
        if self.peek() == Some(first) {
            self.bump();
            Some(both)
        } else {
            let found = match self.peek() {
                Some(c) => format!("'{c}'"),
                None => "end of input".to_string(),
            };
            self.error(
                format!("expected '{first}{first}', found {found}"),
                self.abs_span(start),
            );
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
                    self.error(
                        format!("invalid float literal '{text}'"),
                        self.abs_span(start),
                    );
                    None
                }
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => Some(TokenKind::IntLiteral(n)),
                Err(_) => {
                    self.error(
                        format!("integer literal '{text}' out of range"),
                        self.abs_span(start),
                    );
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
            syntax::KW_REFSTRUCT => TokenKind::RefStruct,
            syntax::KW_VAR => TokenKind::Var,
            syntax::KW_CONST => TokenKind::Const,
            syntax::KW_RETURN => TokenKind::Return,
            syntax::KW_IF => TokenKind::If,
            syntax::KW_ELSE => TokenKind::Else,
            syntax::KW_WHILE => TokenKind::While,
            syntax::KW_FOR => TokenKind::For,
            syntax::KW_IN => TokenKind::In,
            syntax::KW_IMPORT => TokenKind::Import,
            syntax::KW_EXPORT => TokenKind::Export,
            syntax::KW_FROM => TokenKind::From,
            syntax::KW_TRUE => TokenKind::True,
            syntax::KW_FALSE => TokenKind::False,
            syntax::KW_NULL => TokenKind::Null,
            syntax::KW_INT => TokenKind::IntType,
            syntax::KW_FLOAT => TokenKind::FloatType,
            syntax::KW_BOOL => TokenKind::BoolType,
            syntax::KW_STRING => TokenKind::StringType,
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
            vec![
                TokenKind::Fun,
                TokenKind::Identifier("foo".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn refstruct_is_a_keyword_not_an_identifier() {
        assert_eq!(
            kinds("refstruct struct"),
            vec![TokenKind::RefStruct, TokenKind::Struct, TokenKind::Eof]
        );
    }

    #[test]
    fn question_operators_and_null() {
        assert_eq!(
            kinds("? ?? ?. null"),
            vec![
                TokenKind::Question,
                TokenKind::QuestionQuestion,
                TokenKind::QuestionDot,
                TokenKind::Null,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn integer_and_float_literals() {
        assert_eq!(
            kinds("1 2.5"),
            vec![
                TokenKind::IntLiteral(1),
                TokenKind::FloatLiteral(2.5),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn comment_runs_to_end_of_line_on_any_platform() {
        // LF, CRLF, and lone-CR terminated comments each stop before the next line.
        for src in ["fun // c\nx", "fun // c\r\nx", "fun // c\rx"] {
            assert_eq!(
                kinds(src),
                vec![
                    TokenKind::Fun,
                    TokenKind::Identifier("x".into()),
                    TokenKind::Eof
                ],
                "failed for {src:?}"
            );
        }
    }

    #[test]
    fn tabs_and_newlines_are_whitespace() {
        assert_eq!(
            kinds("\tfun\r\nx"),
            vec![
                TokenKind::Fun,
                TokenKind::Identifier("x".into()),
                TokenKind::Eof
            ]
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
    fn lone_ampersand_reports_what_was_found() {
        let (_, diags) = lex("a &x");
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].message.contains("expected '&&', found 'x'"),
            "{diags:?}"
        );
        let (_, diags) = lex("a &");
        assert!(
            diags[0]
                .message
                .contains("expected '&&', found end of input"),
            "{diags:?}"
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

    #[test]
    fn lex_at_offsets_all_spans_by_the_base() {
        let (tokens, diags) = lex_at("fun #", 100);
        assert_eq!(tokens[0].span, Span::new(100, 103));
        assert_eq!(tokens.last().unwrap().span, Span::new(105, 105)); // Eof
        assert_eq!(diags[0].span, Span::new(104, 105)); // the '#'
    }

    #[test]
    fn comparison_operators_single_and_double() {
        assert_eq!(
            kinds("== != <= >= = ! < >"),
            vec![
                TokenKind::EqEq,
                TokenKind::BangEq,
                TokenKind::LessEq,
                TokenKind::GreaterEq,
                TokenKind::Equals,
                TokenKind::Bang,
                TokenKind::Less,
                TokenKind::Greater,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn module_keywords() {
        assert_eq!(
            kinds("import export from"),
            vec![
                TokenKind::Import,
                TokenKind::Export,
                TokenKind::From,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn base_type_and_control_flow_keywords() {
        assert_eq!(
            kinds("true false if else while bool string"),
            vec![
                TokenKind::True,
                TokenKind::False,
                TokenKind::If,
                TokenKind::Else,
                TokenKind::While,
                TokenKind::BoolType,
                TokenKind::StringType,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn string_literal_with_escapes() {
        assert_eq!(
            kinds(r#""a\"b\\c\nd\te""#),
            vec![
                TokenKind::StringLiteral("a\"b\\c\nd\te".to_string()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn unterminated_string_reports_a_diagnostic() {
        let (tokens, diags) = lex("\"abc\nx");
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].message.contains("unterminated string"),
            "{diags:?}"
        );
        // Recovery: lexing continues on the next line.
        assert_eq!(
            tokens.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
            vec![TokenKind::Identifier("x".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn backslash_before_a_newline_is_an_unterminated_string() {
        // Regression: `\` + newline used to report "unknown escape" and let
        // the string swallow the line break, violating single-line semantics.
        let (tokens, diags) = lex("\"ab\\\ncd\"");
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags
            .iter()
            .all(|d| d.message.contains("unterminated string")));
        assert_eq!(
            tokens.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
            vec![TokenKind::Identifier("cd".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn unknown_escape_reports_a_diagnostic_and_recovers() {
        let (tokens, diags) = lex(r#""a\qb""#);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("unknown escape"), "{diags:?}");
        assert_eq!(tokens[0].kind, TokenKind::StringLiteral("aqb".to_string()));
    }

    #[test]
    fn integer_overflow_reports_a_diagnostic_and_recovers() {
        // Too large for i64: a diagnostic plus a placeholder token, not a
        // silent 0 and not a panic.
        let (tokens, diags) = lex("99999999999999999999");
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("out of range"), "{diags:?}");
        assert_eq!(
            tokens.iter().map(|t| t.kind.clone()).collect::<Vec<_>>(),
            vec![TokenKind::IntLiteral(0), TokenKind::Eof]
        );
    }
}
