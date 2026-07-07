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
use std::io::IsTerminal;

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
            print_error(&message);
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
        print_error(&format!("entry file '{entry}' does not define 'main'"));
        std::process::exit(1);
    }

    match interpreter::interpret(&graph, &resolutions) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => exit_on_errors(&[diag], &map),
    }
}

/// True when stderr is a terminal that renders ANSI color. Honors the
/// NO_COLOR convention (disable only when set *and* non-empty, per
/// no-color.org) and `TERM=dumb` terminals, which display escapes as
/// garbage. Piped/redirected output (and the CLI tests) stays plain.
fn use_color() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
}

/// Prints a top-level error (no source span) with the same colored `error:`
/// label as rendered diagnostics, so all error paths look alike.
fn print_error(message: &str) {
    let (sev, reset) = if use_color() {
        (diagnostic::ANSI_ERROR, diagnostic::ANSI_RESET)
    } else {
        ("", "")
    };
    eprintln!("{sev}error{reset}: {message}");
}

/// Renders every diagnostic to stderr and exits nonzero — no-op when empty.
fn exit_on_errors(diags: &[Diagnostic], map: &SourceMap) {
    if diags.is_empty() {
        return;
    }
    let color = use_color();
    for diag in diags {
        eprintln!("{}", diag.render_styled(map, color));
    }
    std::process::exit(1);
}
