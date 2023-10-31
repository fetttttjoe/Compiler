use std::str::Chars;

#[cfg(windows)]
const LINE_ENDING: &'static str = "\r\n";
#[cfg(not(windows))]
const LINE_ENDING: &'static str = "\n";

#[derive(Debug, PartialEq, Clone)]
pub enum Token {
    // Keywords
    Fun,
    Struct,
    IdentifierClosed, // I need a better name for this
    // Constants and variables
    Var,
    Const,
    // Types
    IntType,
    FloatType,
    // Symbols
    Identifier(String),
    // :
    Colon,   
    // {   
    LeftBrace,  
    // }
    RightBrace, 
    // (
    LeftParen,  
    // )
    RightParen, 
    // ,
    Comma,      
    // .
    Dot,        
    // ;
    Semicolon,
    // Operators
    // =
    Equals,
    // +
    Plus,
    // -
    Minus,
    // *
    Asterisk,
    // /
    Slash,
    // %
    Percent,
    // !
    Exclamation,
    // <
    LessThan,
    // >
    GreaterThan,
    // &
    Ampersand,
    // |
    VerticalBar,
    // &&
    DoubleAmpersand,
    // ||
    DoubleVerticalBar,
    // //
    Comment,
    // return
    Return,
    // Line End
    LineEnd,
    IntLiteral(i32),
    Eof,
}

pub struct Lexer<'a> {
    input: Chars<'a>,
    current_char: Option<char>,
    // I am not sure if that is a stupid approach
    brace_counter: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut chars = input.chars();
        let current_char = chars.next();

        Lexer {
            input: chars,
            current_char,
            // I am not sure if that is a stupid approach
            brace_counter: 0,
        }
    }

    fn advance(&mut self) {
        println!("Advancing {:?}", self.current_char);
        self.current_char = self.input.next();
    }

    fn peek(&self) -> Option<char> {
        return self.current_char;
    }


    fn skip_whitespace(&mut self) {
        println!("Skipping whitespace: {:?}", self.peek());
        while let Some(c) = self.peek() {
            if c != ' ' {
                break;
            }
            self.advance();
                // if c.is_whitespace() {
                //     if c == '\r' || c == '\n' || c == ' ' {
                //         break;
                //     } else {
                //         self.advance();
                //     }
                // }
               
            // if !c.is_whitespace() {
            //     break;
            // } 
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
        println!("Next token: {:?}", self.current_char);
        self.skip_whitespace();
        
        if let Some(c) = self.current_char {
            match c {
                ':' => self.consume_single_char(Token::Colon),
                '{' => self.handle_left_brace(),
                '}' => self.handle_right_brace(),
                '(' => self.consume_single_char(Token::LeftParen),
                ')' => self.consume_single_char(Token::RightParen),
                ',' => self.consume_single_char(Token::Comma),
                ';' => self.consume_single_char(Token::Semicolon),
                '.' => self.consume_single_char(Token::Dot),
                '=' => self.consume_single_char(Token::Equals),
                '+' => self.consume_single_char(Token::Plus),
                '-' => self.consume_single_char(Token::Minus),
                '*' => self.consume_single_char(Token::Asterisk),
                '%' => self.consume_single_char(Token::Percent),
                '!' => self.consume_single_char(Token::Exclamation),
                '<' => self.consume_single_char(Token::LessThan),
                '>' => self.consume_single_char(Token::GreaterThan),
                '&' => self.consume_double_symbol('&'),
                '|' =>  self.consume_double_symbol('|'),
                '/' => self.consume_double_symbol('/'),
                '\n' => self.consume_single_char(Token::LineEnd),
                // _ if c == '\r' => self.consume_double_char('\r', Token::LineEnd, Token::Eof),
                 _ if c.is_alphabetic() => { 
                    self.process_identifier()
                },
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
    
    fn handle_left_brace(&mut self) -> Token {
        self.brace_counter += 1;
        self.consume_single_char(Token::LeftBrace)
    }

    fn handle_right_brace(&mut self) -> Token {
        self.brace_counter -= 1;
        if self.brace_counter == 0 {
            self.consume_single_char(Token::IdentifierClosed)
        } else {
            self.consume_single_char(Token::RightBrace)
        }
    }

    fn consume_single_char(&mut self, token: Token) -> Token {
        println!("Consuming single char: {:?}", self.current_char);
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
            '/' => self.consume_double_char('/', Token::Comment, Token::Slash),
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
            "float" => Token::FloatType,
            "return" => Token::Return,
            _ => Token::Identifier(identifier),
        }
    }
    
    pub fn analyse_source(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token();
            println!("Token: {:?}", token);
            if token == Token::Eof {
                break;
            }
            tokens.push(token.clone());
        }
        return tokens;
    }
}
