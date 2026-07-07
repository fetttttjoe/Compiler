mod ast;
mod check;
mod diagnostic;
mod interpreter;
mod lexer;
mod parser;
mod source;
mod span;
mod syntax;
mod token;

use diagnostic::Diagnostic;
use span::LineIndex;

fn main() {
    let source = r#"
fun fib(n: int): int {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}

fun main(): int {
    var i = 0;
    var sum = 0;
    while i < 10 {
        i = i + 1;
        if i % 2 == 0 { sum = sum + fib(i); }
    }
    return sum;
}
"#;

    let index = LineIndex::new(source);

    let (tokens, mut diags) = lexer::lex(source);
    let (ast, parse_diags) = parser::parse(&tokens);
    diags.extend(parse_diags);
    exit_on_errors(&diags, source, &index);

    let (_table, check_diags) = check::check(&ast);
    exit_on_errors(&check_diags, source, &index);

    match interpreter::interpret(&ast) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => exit_on_errors(&[diag], source, &index),
    }
}

/// Renders every diagnostic to stderr and exits nonzero — no-op when empty.
fn exit_on_errors(diags: &[Diagnostic], source: &str, index: &LineIndex) {
    if diags.is_empty() {
        return;
    }
    for diag in diags {
        eprintln!("{}", diag.render(source, index));
    }
    std::process::exit(1);
}
