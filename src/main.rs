mod ast;
mod check;
mod codegen;
mod diagnostic;
mod interpreter;
mod lexer;
mod modules;
mod narrow;
mod parser;
mod source;
mod span;
mod syntax;
mod token;
mod types;

use ast::Item;
use diagnostic::Diagnostic;
use source::SourceMap;
use std::io::{IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `compiler <entry>` interprets; `compiler build <entry> [-o <out>]`
    // compiles to a native binary. `out` defaults to the entry's stem in
    // the current directory.
    let (entry, build_out) = match args.as_slice() {
        [entry] => (entry, None),
        [cmd, entry] if cmd == "build" => (entry, Some(default_out(entry))),
        [cmd, entry, flag, out] if cmd == "build" && flag == "-o" => {
            (entry, Some(std::path::PathBuf::from(out)))
        }
        _ => {
            let _ = writeln!(
                std::io::stderr(),
                "usage: compiler <entry.ys>\n       compiler build <entry.ys> [-o <out>]"
            );
            std::process::exit(2);
        }
    };
    if build_out.as_deref() == Some(std::path::Path::new(entry)) {
        print_error("output binary would overwrite the source file");
        std::process::exit(1);
    }

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

    if let Some(out) = build_out {
        return build(&graph, &out, &map);
    }

    match interpreter::interpret(&graph, &resolutions) {
        Ok((value, heap)) => {
            use std::io::Write;
            if let Err(e) = writeln!(std::io::stdout(), "=> {}", value.render(&heap)) {
                // A closed pipe is fine (the consumer left); losing the
                // result any other way must not look like success.
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    print_error(&format!("cannot write output: {e}"));
                    std::process::exit(1);
                }
            }
        }
        Err(diag) => exit_on_errors(&[diag], &map),
    }
}

/// The default `build` output: the entry's file stem in the current
/// directory (examples/main.ys → ./main).
fn default_out(entry: &str) -> std::path::PathBuf {
    std::path::Path::new(entry)
        .file_stem()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("a.out"))
}

/// Emits assembly next to `out` (kept on disk — it's the debug artifact)
/// and links it through the system `cc`.
fn build(graph: &modules::ModuleGraph, out: &std::path::Path, map: &SourceMap) {
    let asm = match codegen::compile(graph) {
        Ok(asm) => asm,
        Err(diag) => return exit_on_errors(&[diag], map),
    };
    let asm_path = out.with_extension("s");
    if let Err(e) = std::fs::write(&asm_path, asm) {
        print_error(&format!("cannot write {}: {e}", asm_path.display()));
        std::process::exit(1);
    }
    match std::process::Command::new("cc")
        .arg(&asm_path)
        .arg("-o")
        .arg(out)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            print_error(&format!("cc failed ({status})"));
            std::process::exit(1);
        }
        Err(e) => {
            print_error(&format!("cannot run cc: {e}"));
            std::process::exit(1);
        }
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
    let _ = writeln!(std::io::stderr(), "{sev}error{reset}: {message}");
}

/// Renders every diagnostic to stderr and exits nonzero — no-op when empty.
fn exit_on_errors(diags: &[Diagnostic], map: &SourceMap) {
    if diags.is_empty() {
        return;
    }
    let color = use_color();
    for diag in diags {
        let _ = writeln!(std::io::stderr(), "{}", diag.render_styled(map, color));
    }
    std::process::exit(1);
}
