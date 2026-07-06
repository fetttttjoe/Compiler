mod ast;
mod diagnostic;
mod lexer;
mod parser;
mod span;
mod syntax;
mod token;

use span::LineIndex;

fn main() {
    let source = r#"
fun substract(a: int, b: int): int {
    const result = a - b;
    return result;
}
"#;

    let index = LineIndex::new(source);
    let (tokens, diagnostics) = lexer::lex(source);

    for diag in &diagnostics {
        eprintln!("{}", diag.render(&index));
    }
    for token in &tokens {
        println!("{:?}", token.kind);
    }
}
