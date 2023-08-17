// main.rs

mod lexer;

fn main() {
    let source_code = r#"
    struct test {
        a: int,
        b: int,
    }
    
    fun substract(a: int, b: int): int {
        const result = a - b;
        return result;
    }

    fun add(a: int, b: int): int {
        const result = a + b;
        return result;
    }

    fun main() {
        const test = test { a: 1, b: 2 };
        const result = substract(test.a, test.b);
        var a = 1;
        var b = 2;
        const or = a || b;
        const and = a && b;
        const c = add(a, b);
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
    for token in &tokens {
        println!("{:?}", token);
    }
}
