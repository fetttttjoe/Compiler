//! The driver: argument parsing, the load → check → run/build pipeline,
//! and everything that touches the outside world (files, `cc`, exit
//! codes). Compiler passes run on a worker thread with a stack sized
//! for the parser's AST-height bound; the safety guards in `build`
//! protect source files from being overwritten by outputs.

mod ast;
mod check;
mod codegen;
mod diagnostic;
mod interpreter;
mod ir;
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

/// Worker stack for the compiler passes. The parser bounds AST height at
/// MAX_FN_OPS (32_768); the fattest recursive pass (the checker, ~2KB per
/// debug frame) needs ~64MB at that height, so 256MB is 4× headroom. The
/// interpreter still owns its separate 1GB stack with its own eval budget
/// (ADR 0011).
const PIPELINE_STACK_BYTES: usize = 256 << 20;

fn main() {
    match std::thread::Builder::new()
        .name("compiler".into())
        .stack_size(PIPELINE_STACK_BYTES)
        .spawn(run)
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        // Constrained hosts (tight rlimits) may refuse the reservation.
        // ponytail: run on the default stack rather than not at all —
        // only pathologically deep programs could outgrow it there.
        Err(_) => run(),
    }
}

/// How to process the checked program: interpret it, build a native
/// binary, or print pre-register-allocation IR.
enum Mode {
    Interpret { args: Vec<Vec<u8>> },
    Build { out: Option<std::path::PathBuf> },
    Ir,
}

fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (entry, mode) = match args.as_slice() {
        [cmd, rest @ ..] if cmd == "build" => {
            let (entry, out) = parse_build_args(rest);
            (entry, Mode::Build { out })
        }
        [cmd, entry] if cmd == "ir" => (entry, Mode::Ir),
        [cmd, ..] if cmd == "ir" => usage(),
        // Everything after the entry file belongs to the program
        // (ADR 0031); compiled binaries receive argv natively.
        [entry, prog @ ..] => (
            entry,
            Mode::Interpret {
                args: prog.iter().map(|a| a.clone().into_bytes()).collect(),
            },
        ),
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
        Item::Function(f) if f.name == syntax::ENTRY_FN => Some(f),
        _ => None,
    });
    let Some(main_fn) = entry_main else {
        print_error(&format!(
            "entry file '{entry}' does not define '{}'",
            syntax::ENTRY_FN
        ));
        std::process::exit(1);
    };

    match mode {
        Mode::Build { out } => {
            let out = out.unwrap_or_else(|| default_out(entry));
            build(main_fn, &graph, &resolutions, &out, &map)
        }
        Mode::Ir => print_ir(main_fn, &graph, &resolutions, &map),
        Mode::Interpret { args } => match interpreter::interpret(&graph, &resolutions, &args) {
            Ok((value, heap)) => write_stdout(&format!("=> {}\n", value.render(&heap))),
            Err(diag) => exit_on_errors(&[diag], &map),
        },
    }
}

/// `build`'s arguments in any order: exactly one entry file, `-o <out>`
/// anywhere, and `--` ending flag parsing so dashed file names stay
/// reachable. Anything else — unknown flags, a second entry, a dangling
/// `-o` — is a usage error.
fn parse_build_args(rest: &[String]) -> (&String, Option<std::path::PathBuf>) {
    let mut entry = None;
    let mut out = None;
    let mut flags_done = false;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if !flags_done && arg == "--" {
            flags_done = true;
        } else if !flags_done && arg == "-o" {
            match args.next() {
                Some(path) if out.is_none() => out = Some(std::path::PathBuf::from(path)),
                _ => usage(),
            }
        } else if (!flags_done && arg.starts_with('-')) || entry.is_some() {
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
        "usage: compiler <entry.ys>\n       compiler build <entry.ys> [-o <out>]\n       compiler ir <entry.ys>"
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

fn print_ir(
    main_fn: &ast::Function,
    graph: &modules::ModuleGraph,
    resolutions: &check::Resolutions,
    map: &SourceMap,
) {
    match codegen::dump_ir(main_fn, graph, resolutions, map) {
        Ok(ir) => write_stdout(&ir),
        Err(diag) => exit_on_errors(&[diag], map),
    }
}

fn write_stdout(text: &str) {
    let mut stdout = std::io::stdout();
    if let Err(e) = stdout.write_all(text.as_bytes())
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        print_error(&format!("cannot write output: {e}"));
        std::process::exit(1);
    }
}

/// Emits assembly next to `out` (kept on disk — it's the debug artifact)
/// and links it through the system `cc`.
fn build(
    main_fn: &ast::Function,
    graph: &modules::ModuleGraph,
    resolutions: &check::Resolutions,
    out: &std::path::Path,
    map: &SourceMap,
) {
    let asm = match codegen::compile(main_fn, graph, resolutions, map) {
        Ok(asm) => asm,
        Err(diag) => return exit_on_errors(&[diag], map),
    };
    let asm_path = out.with_extension("s");
    // `with_extension` is the identity on `.s` names: cc's input and
    // output would be the same file.
    if asm_path == out {
        print_error(&format!(
            "output '{}' collides with its assembly file — pick a name not ending in .s",
            out.display()
        ));
        std::process::exit(1);
    }
    // The assembly path is derived, not user-chosen — never write it
    // through a pre-existing symlink to somewhere the user didn't name.
    if std::fs::symlink_metadata(&asm_path).is_ok_and(|m| m.file_type().is_symlink()) {
        print_error(&format!(
            "assembly path '{}' is a symlink — refusing to write through it",
            asm_path.display()
        ));
        std::process::exit(1);
    }
    // Never write over a loaded source file. Same-file means same inode,
    // so `./` spellings, symlink chains, and hard links can't disguise
    // one; a target that doesn't exist yet can't destroy anything.
    for target in [asm_path.as_path(), out] {
        if graph
            .modules
            .iter()
            .any(|m| same_file(target, std::path::Path::new(&m.path)))
        {
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
    // -lm: float `%` compiles to fmod, which lives in libm.
    match std::process::Command::new("cc")
        .arg(&asm_path)
        .arg("-o")
        .arg(out)
        .arg("-lm")
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

/// True when both paths name the same existing file (device + inode).
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
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
