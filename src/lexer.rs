use std::str::Chars;

#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    Fun,
    Struct,
    Var,
    IntType,
    Identifier(String),
    Colon,
    LeftBrace,
    RightBrace,
    LeftParen,
    RightParen,
    Comma,
    Equals,
    Return,
    IntLiteral(i32),
    Eof,
}

pub struct Lexer<'a> {
    input: Chars<'a>,
    current_char: Option<char>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut chars = input.chars();
        let current_char = chars.next();

        Lexer {
            input: chars,
            current_char,
        }
    }

    fn advance(&mut self) {
        self.current_char = self.input.next();
    }

    fn peek(&self) -> Option<char> {
        return self.current_char;
    }


    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if !c.is_whitespace() {
                break;
            }
            self.advance();
        }
    }

    fn read_identifier(&mut self) -> String {
        let mut identifier = String::new();
        while let Some(c) = self.current_char {
            if c.is_alphanumeric() || c == '_' {
                identifier.push(c);
                self.advance();
            } else {
                break;
            }
        }
        return identifier;
    }

    fn read_number(&mut self) -> i32 {
        let mut number_str = String::new();
        while let Some(c) = self.current_char {
            if c.is_digit(10) {
                number_str.push(c);
                self.advance();
            } else {
                break;
            }
        }
        return number_str.parse::<i32>().unwrap_or(0);
    }

    pub fn next_token(&mut self) -> Token {
        self.skip_whitespace();
    
        if let Some(c) = self.current_char {
            match c {
                ':' => {
                    self.advance();
                    Token::Colon
                }
                '{' => {
                    self.advance();
                    Token::LeftBrace
                }
                '}' => {
                    self.advance();
                    Token::RightBrace
                }
                '(' => {
                    self.advance();
                    Token::LeftParen
                }
                ')' => {
                    self.advance();
                    Token::RightParen
                }
                ',' => {
                    self.advance();
                    Token::Comma
                }
                '=' => {
                    self.advance();
                    Token::Equals
                }
                _ if c.is_alphabetic() => {
                    let identifier = self.read_identifier();
                    match identifier.trim() {
                        "fun" => Token::Fun,
                        "struct" => Token::Struct,
                        "var" => Token::Var,
                        "int" => Token::IntType,
                        "return" => Token::Return,
                        _ => Token::Identifier(identifier),
                    }
                }
                _ if c.is_digit(10) => {
                    let number = self.read_number();
                    Token::IntLiteral(number)
                }
                _ => Token::Eof,
            }
        } else {
            return Token::Eof;
        }
    }
    
}
