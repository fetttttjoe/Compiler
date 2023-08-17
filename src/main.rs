// main.rs

mod lexer;

fn main() {
    let source_code = r#"
    struct test {
        a: int,
        b: int,
    }
        fun add(a: int, b: int): int {
            var result = a + b;
            return result;
        }
    "#;

    let mut lexer = lexer::Lexer::new(source_code);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        if token == lexer::Token::Eof {
            break;
        }
        tokens.push(token);
    }
    println!("{:?}", tokens);
}
