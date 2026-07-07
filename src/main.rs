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

use ast::Ast;
use diagnostic::Diagnostic;
use source::SourceMap;

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: compiler <file>...");
        std::process::exit(2);
    }

    let mut map = SourceMap::new();
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                map.add(path.clone(), text);
            }
            Err(e) => {
                eprintln!("error: cannot read '{path}': {e}");
                std::process::exit(1);
            }
        }
    }

    let (ast, diags) = front_end(&map);
    exit_on_errors(&diags, &map);

    let (_table, check_diags) = check::check(&ast);
    exit_on_errors(&check_diags, &map);

    match interpreter::interpret(&ast) {
        Ok(value) => println!("=> {value:?}"),
        Err(diag) => exit_on_errors(&[diag], &map),
    }
}

/// Lexes and parses every file in parallel — the phases are pure and the
/// results owned, so this is safe by construction. Files are chunked across
/// at most `available_parallelism` workers (thread-per-file would fall over
/// on huge projects). Items merge in file order; diagnostics sort by global
/// position, so output is deterministic regardless of scheduling.
fn front_end(map: &SourceMap) -> (Ast, Vec<Diagnostic>) {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let chunk_size = map.files().len().div_ceil(workers).max(1);

    let results: Vec<Vec<(Ast, Vec<Diagnostic>)>> = std::thread::scope(|s| {
        let handles: Vec<_> = map
            .files()
            .chunks(chunk_size)
            .map(|files| {
                s.spawn(move || {
                    files
                        .iter()
                        .map(|file| {
                            let (tokens, mut diags) =
                                lexer::lex_at(file.text(), file.base());
                            let (items, parse_diags) = parser::parse(&tokens);
                            diags.extend(parse_diags);
                            (items, diags)
                        })
                        .collect()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("front-end worker panicked"))
            .collect()
    });

    let mut ast = Vec::new();
    let mut diags = Vec::new();
    for (items, file_diags) in results.into_iter().flatten() {
        ast.extend(items);
        diags.extend(file_diags);
    }
    diags.sort_by_key(|d| (d.span.start, d.span.end));
    (ast, diags)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_file_calls_compile_and_run() {
        let mut map = SourceMap::new();
        map.add("lib.lang", "fun forty(): int { return 40; }");
        map.add("main.lang", "fun main(): int { return forty() + 2; }");
        let (ast, diags) = front_end(&map);
        assert!(diags.is_empty(), "{diags:?}");
        let (_t, cd) = check::check(&ast);
        assert!(cd.is_empty(), "{cd:?}");
        assert_eq!(
            interpreter::interpret(&ast),
            Ok(interpreter::Value::Int(42))
        );
    }

    #[test]
    fn duplicate_functions_across_files_are_reported() {
        let mut map = SourceMap::new();
        map.add("a.lang", "fun f(): int { return 1; }");
        map.add("b.lang", "fun f(): int { return 2; }");
        let (ast, diags) = front_end(&map);
        assert!(diags.is_empty(), "{diags:?}");
        let (_t, cd) = check::check(&ast);
        assert!(
            cd.iter().any(|d| d.message.contains("'f' is already defined")),
            "{cd:?}"
        );
    }

    #[test]
    fn diagnostics_are_sorted_by_source_position_across_files() {
        let mut map = SourceMap::new();
        map.add("a.lang", "fun f(): int { return 1; } #");
        map.add("b.lang", "@ fun g(): int { return 2; }");
        let (_ast, diags) = front_end(&map);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags[0].span.start < diags[1].span.start, "a.lang first");
    }
}
