mod ast;
mod check;
mod diagnostic;
mod interpreter;
mod lexer;
mod modules;
mod parser;
mod source;
mod span;
mod syntax;
mod token;

use ast::Item;
use diagnostic::Diagnostic;
use source::SourceMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [entry] = args.as_slice() else {
        eprintln!("usage: compiler <entry.ys>");
        std::process::exit(2);
    };

    let mut map = SourceMap::new();
    let mut read = |path: &str| std::fs::read_to_string(path).map_err(|e| e.to_string());
    let (graph, diags) = match modules::load_program(entry, &mut read, &mut map) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    };
    exit_on_errors(&diags, &map);

    let (resolutions, check_diags) = check::check(&graph);
    exit_on_errors(&check_diags, &map);

    let entry_has_main = graph.modules[0]
        .ast
        .iter()
        .any(|item| matches!(item, Item::Function(f) if f.name == "main"));
    if !entry_has_main {
        eprintln!("error: entry file '{entry}' does not define 'main'");
        std::process::exit(1);
    }

    match interpreter::interpret(&graph, &resolutions) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => exit_on_errors(&[diag], &map),
    }
}

/// Renders every diagnostic to stderr and exits nonzero — no-op when empty.
fn exit_on_errors(diags: &[Diagnostic], map: &SourceMap) {
    if diags.is_empty() {
        return;
    }
    for diag in diags {
        eprintln!("{}", diag.render(map));
    }
    std::process::exit(1);
}
