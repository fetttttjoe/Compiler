use std::str::Chars;
#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    // Keywords
    Fun,
    Struct,
    // Special
    Main,
    // Constants and variables
    Var,
    Const,
    // Types
    IntType,
    // Symbols
    Identifier(String),
    Colon,
    LeftBrace,
    RightBrace,
    LeftParen,
    RightParen,
    Comma,
    Dot,
    Semicolon,
    Return,
    IntLiteral(i32),
    Eof,
    // Operators
    Equals,
    Plus,
    Minus,
    Asterisk,
    Slash,
    Percent,
    Exclamation,
    LessThan,
    GreaterThan,
    Ampersand,
    VerticalBar,
    DoubleAmpersand,
    DoubleVerticalBar,
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
                ':' => self.consume_single_char(Token::Colon),
                '{' => self.consume_single_char(Token::LeftBrace),
                '}' => self.consume_single_char(Token::RightBrace),
                '(' => self.consume_single_char(Token::LeftParen),
                ')' => self.consume_single_char(Token::RightParen),
                ',' => self.consume_single_char(Token::Comma),
                ';' => self.consume_single_char(Token::Semicolon),
                '.' => self.consume_single_char(Token::Dot),
                '=' => self.consume_single_char(Token::Equals),
                '+' => self.consume_single_char(Token::Plus),
                '-' => self.consume_single_char(Token::Minus),
                '*' => self.consume_single_char(Token::Asterisk),
                '/' => self.consume_single_char(Token::Slash),
                '%' => self.consume_single_char(Token::Percent),
                '!' => self.consume_single_char(Token::Exclamation),
                '<' => self.consume_single_char(Token::LessThan),
                '>' => self.consume_single_char(Token::GreaterThan),
                '&' => self.consume_double_symbol('&'),
                '|' =>  self.consume_double_symbol('|'),
                 _ if c.is_alphabetic() =>{ 
                    println!("Processing alphabetic character: '{}', ASCII: {}", c, c as u8);
                    self.process_identifier()},
                _ if c.is_digit(10) => {
                    let number = self.read_number();
                    return Token::IntLiteral(number);
                }
                _ => Token::Eof,
            }
        } else {
            return Token::Eof;
        }
    }
    fn consume_single_char(&mut self, token: Token) -> Token {
        self.advance();
        return token;
    }

    fn consume_double_char(&mut self, expected: char, token1: Token, token2: Token) -> Token {
        self.advance();
        if let Some(next_char) = self.peek() {
            if next_char == expected {
                self.advance();
                return token1;
            }
        }
        return token2;
    }
    
    fn consume_double_symbol(&mut self, symbol: char) -> Token {
        match symbol {
            '&' => self.consume_double_char('&', Token::DoubleAmpersand, Token::Ampersand),
            '|' => self.consume_double_char('|', Token::DoubleVerticalBar, Token::VerticalBar),
            _ => panic!("Unsupported double symbol: {}", symbol),
        }
    }
    fn process_identifier(&mut self) -> Token {
        let identifier = self.read_identifier();
        match identifier.trim() {
            "fun" => Token::Fun,
            "struct" => Token::Struct,
            "var" => Token::Var,
            "const" => Token::Const,
            "int" => Token::IntType,
            "return" => Token::Return,
            "main" => Token::Main,
            _ => Token::Identifier(identifier),
        }
    }
    
}
