use std::collections::HashMap;

use crate::ast::{BinOp, Conv, Expr, Function, Item, Stmt, UnOp};
use crate::check::Resolutions;
use crate::diagnostic::Diagnostic;
use crate::modules::ModuleGraph;
use crate::span::Span;
use crate::syntax;

// ---- Interpreter policy ----------------------------------------------
// One unit of evaluation depth (a call, statement, or expression level)
// costs at most ~16KB of native stack in debug builds (measured); the
// owned stack is sized so the depth budget always binds first:
// 65_536 units x 16KB = 1GB = INTERP_STACK_BYTES. The heap cap turns
// runaway allocation into a diagnostic instead of an OOM kill.
const MAX_EVAL_DEPTH: usize = 65_536;
const MAX_HEAP_CELLS: usize = 1 << 20;
const INTERP_STACK_BYTES: usize = 1 << 30;

/// The interpreter's arena: every refstruct object and array buffer lives
/// here, addressed by handle into its own typed table — a `Value::Ref` can
/// only name a struct object and a `Value::Array` only a buffer, so no
/// mismatch arm exists anywhere. Nothing is freed mid-run; the arena drops
/// wholesale when execution ends (ADR 0009's collector-free story), which
/// also makes reference cycles harmless.
#[derive(Debug, Default)]
pub struct Heap {
    structs: Vec<StructObj>,
    arrays: Vec<Vec<Value>>,
}

/// A refstruct object; fields sorted by name like inline structs.
#[derive(Debug)]
struct StructObj {
    name: String,
    fields: Vec<(String, Value)>,
}

impl Heap {
    fn cell_count(&self) -> usize {
        self.structs.len() + self.arrays.len()
    }
}

/// Handles are plain indices, so the derived `PartialEq` gives refstructs
/// and arrays identity equality for free, and `Value` stays `Send` — which
/// lets the interpreter own its execution stack (see `interpret`).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Raw length-carried bytes — the ADR 0013 representation. Source
    /// literals are always valid UTF-8, but file input (ADR 0031) need
    /// not be, and both engines pass bytes through untouched.
    Str(Vec<u8>),
    /// Fields sorted by name (literals may write them in any order, and
    /// sorting makes `PartialEq` order-independent); lookup is a linear
    /// scan — structs are small, and the checker guarantees the field
    /// exists. NOTE: the sort is observable language spec, not just a
    /// convenience — `print` renders fields in this (name-sorted) order
    /// and the differential harness compares stdout byte-for-byte, so a
    /// future codegen struct renderer must sort by name too, NOT walk
    /// declaration-order layout.
    Struct {
        name: String,
        fields: Vec<(String, Value)>,
    },
    /// A `refstruct` instance: a handle to one shared heap object, aliased
    /// by every copy of the handle.
    Ref(usize),
    /// An array: a handle to one shared, growable heap buffer.
    Array(usize),
    /// The `null` literal — the empty state of a `T?` slot.
    Null,
    Unit,
}

mod eval;
mod render;
#[cfg(test)]
mod tests;

/// Runs `main()` from the entry module (graph index 0), resolving every call
/// through its module's alias map. Returns `Unit` (and the heap, for
/// rendering the result) when there is no `main`. Execution happens on the
/// interpreter's own thread — `Value` is `Send` because handles are plain
/// arena indices.
pub fn interpret(
    graph: &ModuleGraph,
    resolutions: &Resolutions,
) -> Result<(Value, Heap), Diagnostic> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("interpreter".to_string())
            .stack_size(INTERP_STACK_BYTES)
            .spawn_scoped(scope, || eval::run_program(graph, resolutions));
        match worker {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            // Constrained hosts (strict overcommit, tight rlimits) can
            // refuse the stack reservation — that's an error, not a panic.
            Err(e) => Err(Diagnostic::error(
                format!("cannot start the interpreter: {e}"),
                Span::new(0, 0),
            )),
        }
    })
}
