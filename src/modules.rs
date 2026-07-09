//! Multi-file programs: starting from the entry file, discover each
//! wave of `import`s, lex+parse the wave in parallel, repeat until the
//! graph closes, then reject cycles. Modules are numbered in discovery
//! order — the entry file is always index 0, which keeps everything
//! downstream deterministic.

use std::collections::HashMap;
use std::path::{Component, Path};

use crate::ast::{Ast, Item};
use crate::diagnostic::Diagnostic;
use crate::source::SourceMap;
use crate::span::Span;
use crate::{lexer, parser};

/// One imported name: `name` (at `span`) resolved from the module at graph
/// index `target`.
#[derive(Debug)]
pub struct ImportBinding {
    pub name: String,
    pub span: Span,
    pub target: usize,
}

/// A loaded module: its canonical path, parsed items, and import bindings.
pub struct Module {
    pub path: String,
    pub ast: Ast,
    pub imports: Vec<ImportBinding>,
}

/// The program's import graph. Modules are numbered in discovery order —
/// the entry file is always index 0 — which keeps output deterministic.
pub struct ModuleGraph {
    pub modules: Vec<Module>,
}

/// Loads a program starting at `entry`: parse it, discover its imports, load
/// that wave of files (lexed+parsed in parallel), repeat until the graph is
/// closed, then reject import cycles.
///
/// `read` abstracts the filesystem so tests run on in-memory files. An
/// unreadable *entry* is `Err` (there is no source location to point at);
/// unreadable *imports* are ordinary diagnostics at the import's path.
pub fn load_program(
    entry: &str,
    read: &mut dyn FnMut(&str) -> Result<String, String>,
    map: &mut SourceMap,
) -> Result<(ModuleGraph, Vec<Diagnostic>), String> {
    let mut diags = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut paths = vec![entry.to_string()];
    let mut requested_at: Vec<Option<Span>> = vec![None];
    let mut slots: Vec<Option<Module>> = vec![None];
    index_of.insert(entry.to_string(), 0);

    let mut wave = vec![0usize];
    while !wave.is_empty() {
        // 1) Read and register this wave's sources (sequential I/O).
        let mut loaded: Vec<(usize, usize)> = Vec::new(); // (module, file index)
        for &mi in &wave {
            match read(&paths[mi]) {
                Ok(text) => {
                    map.add(paths[mi].clone(), text);
                    loaded.push((mi, map.files().len() - 1));
                }
                Err(e) => {
                    let msg = format!("cannot read module '{}': {e}", paths[mi]);
                    match requested_at[mi] {
                        Some(span) => diags.push(Diagnostic::error(msg, span)),
                        None => return Err(msg), // the entry file itself
                    }
                    slots[mi] = Some(Module {
                        path: paths[mi].clone(),
                        ast: Vec::new(),
                        imports: Vec::new(),
                    });
                }
            }
        }

        // 2) Lex + parse the wave in parallel (same bounded-worker shape as
        //    the rest of the front-end; results rejoin in spawn order, so
        //    everything stays deterministic).
        let map_ref: &SourceMap = map;
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let chunk_size = loaded.len().div_ceil(workers).max(1);
        let parsed: Vec<(usize, Ast, Vec<Diagnostic>)> = std::thread::scope(|s| {
            let handles: Vec<_> = loaded
                .chunks(chunk_size)
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .iter()
                            .map(|&(mi, fi)| {
                                let file = &map_ref.files()[fi];
                                let (tokens, mut file_diags) =
                                    lexer::lex_at(file.text(), file.base());
                                let (ast, parse_diags) = parser::parse(&tokens);
                                file_diags.extend(parse_diags);
                                (mi, ast, file_diags)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("module worker panicked"))
                .collect()
        });

        // 3) Record modules, resolve import paths, queue the next wave.
        let mut next_wave = Vec::new();
        for (mi, ast, file_diags) in parsed {
            diags.extend(file_diags);
            let mut bindings = Vec::new();
            for item in &ast {
                let Item::Import(imp) = item else { continue };
                if imp.path.is_empty() {
                    continue; // the parser already reported the broken path
                }
                let target_path = resolve_path(&paths[mi], &imp.path);
                let target = match index_of.get(&target_path) {
                    Some(&i) => i,
                    None => {
                        let i = slots.len();
                        index_of.insert(target_path.clone(), i);
                        paths.push(target_path);
                        requested_at.push(Some(imp.path_span));
                        slots.push(None);
                        next_wave.push(i);
                        i
                    }
                };
                for (name, span) in &imp.names {
                    bindings.push(ImportBinding {
                        name: name.clone(),
                        span: *span,
                        target,
                    });
                }
            }
            slots[mi] = Some(Module {
                path: paths[mi].clone(),
                ast,
                imports: bindings,
            });
        }
        wave = next_wave;
    }

    let modules: Vec<Module> = slots
        .into_iter()
        .map(|m| m.expect("every discovered module is loaded"))
        .collect();

    if let Some(cycle) = detect_cycle(&modules) {
        diags.push(cycle);
    }
    diags.sort_by_key(|d| (d.span.start, d.span.end));
    Ok((ModuleGraph { modules }, diags))
}

/// Resolves an import path relative to the importing file, lexically
/// (`.`/`..` folded, no filesystem access); appends `.ys` when missing.
fn resolve_path(importer: &str, import: &str) -> String {
    let mut parts: Vec<String> = Path::new(importer)
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    for component in Path::new(import).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    let mut path = parts.join("/");
    if !path.ends_with(".ys") {
        path.push_str(".ys");
    }
    path
}

/// Rejects import cycles: depth-first search over the import edges; a back
/// edge to a module still on the stack is a cycle, reported with its path.
fn detect_cycle(modules: &[Module]) -> Option<Diagnostic> {
    const UNSEEN: u8 = 0;
    const ON_STACK: u8 = 1;
    const DONE: u8 = 2;

    fn dfs(
        v: usize,
        modules: &[Module],
        color: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<(usize, Span)> {
        color[v] = ON_STACK;
        stack.push(v);
        for binding in &modules[v].imports {
            match color[binding.target] {
                ON_STACK => return Some((binding.target, binding.span)),
                UNSEEN => {
                    if let Some(found) = dfs(binding.target, modules, color, stack) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        color[v] = DONE;
        stack.pop();
        None
    }

    let mut color = vec![UNSEEN; modules.len()];
    let mut stack = Vec::new();
    for v in 0..modules.len() {
        if color[v] == UNSEEN {
            if let Some((back_to, span)) = dfs(v, modules, &mut color, &mut stack) {
                let from = stack.iter().position(|&m| m == back_to).unwrap_or(0);
                let mut names: Vec<&str> = stack[from..]
                    .iter()
                    .map(|&m| modules[m].path.as_str())
                    .collect();
                names.push(&modules[back_to].path);
                return Some(Diagnostic::error(
                    format!("import cycle: {}", names.join(" → ")),
                    span,
                ));
            }
            stack.clear();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(entry: &str, files: &[(&str, &str)]) -> Result<(ModuleGraph, Vec<Diagnostic>), String> {
        let store: HashMap<String, String> = files
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut read = |path: &str| {
            store
                .get(path)
                .cloned()
                .ok_or_else(|| "no such file".to_string())
        };
        let mut map = SourceMap::new();
        load_program(entry, &mut read, &mut map)
    }

    #[test]
    fn discovers_transitive_imports_in_order() {
        let (graph, diags) = load(
            "main.ys",
            &[
                (
                    "main.ys",
                    "import { a } from \"./a\"; fun main(): int { return a(); }",
                ),
                (
                    "a.ys",
                    "import { b } from \"./b\"; export fun a(): int { return b(); }",
                ),
                ("b.ys", "export fun b(): int { return 1; }"),
            ],
        )
        .unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        let paths: Vec<&str> = graph.modules.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["main.ys", "a.ys", "b.ys"]);
        assert_eq!(graph.modules[0].imports[0].target, 1);
        assert_eq!(graph.modules[1].imports[0].target, 2);
    }

    #[test]
    fn diamond_dependencies_load_once() {
        let (graph, diags) = load(
            "main.ys",
            &[
                (
                    "main.ys",
                    "import { a } from \"./a\"; import { b } from \"./b\";\n\
                     fun main(): int { return a() + b(); }",
                ),
                (
                    "a.ys",
                    "import { s } from \"./shared\"; export fun a(): int { return s(); }",
                ),
                (
                    "b.ys",
                    "import { s } from \"./shared\"; export fun b(): int { return s(); }",
                ),
                ("shared.ys", "export fun s(): int { return 1; }"),
            ],
        )
        .unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(graph.modules.len(), 4);
        // Both a and b point at the same shared module.
        assert_eq!(
            graph.modules[1].imports[0].target,
            graph.modules[2].imports[0].target
        );
    }

    #[test]
    fn parent_relative_paths_resolve_lexically() {
        let (graph, diags) = load(
            "app/main.ys",
            &[
                (
                    "app/main.ys",
                    "import { x } from \"../lib/x\"; fun main(): int { return x(); }",
                ),
                ("lib/x.ys", "export fun x(): int { return 1; }"),
            ],
        )
        .unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(graph.modules[1].path, "lib/x.ys");
    }

    #[test]
    fn missing_module_is_reported_at_the_import_path() {
        let (graph, diags) = load(
            "main.ys",
            &[(
                "main.ys",
                "import { f } from \"./missing\"; fun main(): int { return 1; }",
            )],
        )
        .unwrap();
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("cannot read module 'missing.ys'"),
            "{diags:?}"
        );
        assert_eq!(graph.modules.len(), 2); // entry + the empty placeholder
    }

    #[test]
    fn unreadable_entry_is_a_hard_error() {
        assert!(load("nope.ys", &[]).is_err());
    }

    #[test]
    fn import_cycles_are_rejected_with_their_path() {
        let (_, diags) = load(
            "a.ys",
            &[
                (
                    "a.ys",
                    "import { b } from \"./b\"; export fun a(): int { return 1; }",
                ),
                (
                    "b.ys",
                    "import { a } from \"./a\"; export fun b(): int { return 1; }",
                ),
            ],
        )
        .unwrap();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("import cycle: a.ys → b.ys → a.ys")),
            "{diags:?}"
        );
    }
}
