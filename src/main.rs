mod ast;
mod check;
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
    let (tokens, lex_diags) = lexer::lex(source);
    let (ast, parse_diags) = parser::parse(&tokens);

    let mut had_error = false;
    for diag in lex_diags.iter().chain(parse_diags.iter()) {
        eprintln!("{}", diag.render(&index));
        had_error = true;
    }
    if had_error {
        std::process::exit(1);
    }

    println!("{ast:#?}");
}
