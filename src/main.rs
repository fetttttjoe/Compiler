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
    // Every pass recurses over the AST, so the pipeline runs on a worker
    // with the same generous stack the interpreter gets (ADR 0011). The
    // checker's expression-depth budget (check::MAX_EXPR_DEPTH) turns
    // deep programs into diagnostics long before this stack runs out.
    std::thread::Builder::new()
        .name("compiler".into())
        .stack_size(1 << 30)
        .spawn(run)
        .expect("cannot spawn the compiler thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `compiler <entry>` interprets; `compiler build <entry> [-o <out>]`
    // compiles to a native binary. `build_out`: None = interpret,
    // Some(None) = build to the default output, Some(Some(p)) = build -o p.
    let (entry, build_out) = match args.as_slice() {
        [cmd, rest @ ..] if cmd == "build" => {
            let (entry, out) = parse_build_args(rest);
            (entry, Some(out))
        }
        [entry] => (entry, None),
        _ => usage(),
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

    let entry_main = graph.modules[0].ast.iter().find_map(|item| match item {
        Item::Function(f) if f.name == "main" => Some(f),
        _ => None,
    });
    let Some(main_fn) = entry_main else {
        print_error(&format!("entry file '{entry}' does not define 'main'"));
        std::process::exit(1);
    };

    if let Some(out_flag) = build_out {
        let out = out_flag.unwrap_or_else(|| default_out(entry));
        return build(main_fn, &graph, &out, &map);
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

/// `build`'s arguments in any order: exactly one entry file, `-o <out>`
/// anywhere. Anything else — unknown flags, a second entry, a dangling
/// `-o` — is a usage error.
fn parse_build_args(rest: &[String]) -> (&String, Option<std::path::PathBuf>) {
    let mut entry = None;
    let mut out = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "-o" {
            match args.next() {
                Some(path) if out.is_none() => out = Some(std::path::PathBuf::from(path)),
                _ => usage(),
            }
        } else if arg.starts_with('-') || entry.is_some() {
            usage();
        } else {
            entry = Some(arg);
        }
    }
    match entry {
        Some(entry) => (entry, out),
        None => usage(),
    }
}

fn usage() -> ! {
    let _ = writeln!(
        std::io::stderr(),
        "usage: compiler <entry.ys>\n       compiler build <entry.ys> [-o <out>]"
    );
    std::process::exit(2);
}

/// The default `build` output: the entry's file stem in the current
/// directory (examples/main.ys → ./main). Resolved only after the entry
/// was read successfully, so it always has a file name.
fn default_out(entry: &str) -> std::path::PathBuf {
    std::path::Path::new(entry)
        .file_stem()
        .map(std::path::PathBuf::from)
        .expect("a readable entry file has a name")
}

/// Emits assembly next to `out` (kept on disk — it's the debug artifact)
/// and links it through the system `cc`.
fn build(
    main_fn: &ast::Function,
    graph: &modules::ModuleGraph,
    out: &std::path::Path,
    map: &SourceMap,
) {
    let asm = match codegen::compile(main_fn) {
        Ok(asm) => asm,
        Err(diag) => return exit_on_errors(&[diag], map),
    };
    let asm_path = out.with_extension("s");
    // Refuse to write over any loaded source file, whatever spelling the
    // paths use — canonicalize resolves `./`, `..`, and symlinks. A target
    // that doesn't exist yet can't clobber anything (canonicalize errs).
    let sources: Vec<std::path::PathBuf> = graph
        .modules
        .iter()
        .filter_map(|m| std::fs::canonicalize(&m.path).ok())
        .collect();
    for target in [asm_path.as_path(), out] {
        if std::fs::canonicalize(target).is_ok_and(|t| sources.contains(&t)) {
            print_error(&format!(
                "refusing to overwrite source file '{}'",
                target.display()
            ));
            std::process::exit(1);
        }
    }
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
