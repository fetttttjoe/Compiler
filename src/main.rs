mod ast;
mod check;
mod diagnostic;
mod interpreter;
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

fun main(): int {
    return substract(10, 4);
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

    let (_table, check_diags) = check::check(&ast);
    if !check_diags.is_empty() {
        for diag in &check_diags {
            eprintln!("{}", diag.render(&index));
        }
        std::process::exit(1);
    }

    match interpreter::interpret(&ast) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => {
            eprintln!("{}", diag.render(&index));
            std::process::exit(1);
        }
    }
}
