mod ast;
mod check;
mod diagnostic;
mod interpreter;
mod lexer;
mod parser;
mod span;
mod syntax;
mod token;

use diagnostic::Diagnostic;
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

    let (tokens, mut diags) = lexer::lex(source);
    let (ast, parse_diags) = parser::parse(&tokens);
    diags.extend(parse_diags);
    exit_on_errors(&diags, &index);

    let (_table, check_diags) = check::check(&ast);
    exit_on_errors(&check_diags, &index);

    match interpreter::interpret(&ast) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => exit_on_errors(&[diag], &index),
    }
}

/// Renders every diagnostic to stderr and exits nonzero — no-op when empty.
fn exit_on_errors(diags: &[Diagnostic], index: &LineIndex) {
    if diags.is_empty() {
        return;
    }
    for diag in diags {
        eprintln!("{}", diag.render(index));
    }
    std::process::exit(1);
}
