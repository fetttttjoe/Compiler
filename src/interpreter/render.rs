//! Turning values into text: `display` is what `print` writes (scalars
//! and strings raw, aggregates source-like), `render` the debug-style
//! result line. `display` produces BYTES — strings are raw bytes
//! (ADR 0013), so non-UTF-8 file input passes through untouched; only
//! the oracle-only `render` result line goes lossy. Struct fields
//! render in name-sorted storage order — observable language spec
//! pinned by the conformance corpus, so a future compiled renderer
//! must sort the same way. Both are depth-capped: cyclic refstructs
//! would recurse forever otherwise.

use super::*;

impl Value {
    /// Human-facing rendering for `print`: scalars and strings raw, structs
    /// and arrays in source-like shape, refs printed through the handle.
    /// Depth-capped like `render` — a handle hop costs a level, so cycles
    /// stay as bounded as they were under the Rc oracle.
    pub fn display(&self, heap: &Heap) -> Vec<u8> {
        self.display_depth(heap, 8)
    }

    fn display_depth(&self, heap: &Heap, depth: usize) -> Vec<u8> {
        if depth == 0 {
            return b"...".to_vec();
        }
        match self {
            Value::Int(n) => n.to_string().into_bytes(),
            Value::Float(f) => f.to_string().into_bytes(),
            Value::Bool(b) => b.to_string().into_bytes(),
            Value::Str(s) => s.clone(),
            Value::Null => b"null".to_vec(),
            Value::Unit => b"unit".to_vec(),
            // The hop consumes a level, the object's children another.
            Value::Ref(id) if depth == 1 => {
                let _ = id;
                b"...".to_vec()
            }
            Value::Ref(id) => {
                let obj = &heap.structs[*id];
                display_struct(&obj.name, &obj.fields, |v| v.display_depth(heap, depth - 2))
            }
            Value::Array(id) => {
                display_items(&heap.arrays[*id], |v| v.display_depth(heap, depth - 1))
            }
            Value::Struct { name, fields } => {
                display_struct(name, fields, |v| v.display_depth(heap, depth - 1))
            }
        }
    }

    /// Debug-style rendering with a depth cap — cyclic refstruct values
    /// would recurse forever otherwise. Oracle-only (the `=>` result
    /// line), so binary strings may render lossily here.
    // ponytail: depth cap, not cycle detection — 8 levels is plenty for a
    // result dump; switch to handle-tracking if real output needs it.
    pub fn render(&self, heap: &Heap) -> String {
        self.render_depth(heap, 8)
    }

    fn render_depth(&self, heap: &Heap, depth: usize) -> String {
        if depth == 0 {
            return "...".to_string();
        }
        match self {
            Value::Ref(_) if depth == 1 => "...".to_string(),
            Value::Ref(id) => {
                let obj = &heap.structs[*id];
                format!(
                    "Ref({})",
                    render_struct(&obj.name, &obj.fields, |v| v.render_depth(heap, depth - 2))
                )
            }
            Value::Array(id) => {
                render_items(&heap.arrays[*id], |v| v.render_depth(heap, depth - 1))
            }
            Value::Struct { name, fields } => {
                render_struct(name, fields, |v| v.render_depth(heap, depth - 1))
            }
            Value::Str(s) => format!("Str({:?})", String::from_utf8_lossy(s)),
            other => format!("{other:?}"),
        }
    }
    pub(super) fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "string",
            Value::Struct { .. } => "struct",
            Value::Ref(_) => "refstruct",
            Value::Array(_) => "array",
            Value::Null => "null",
            Value::Unit => "unit",
        }
    }
}

fn display_struct(
    name: &str,
    fields: &[(String, Value)],
    mut one: impl FnMut(&Value) -> Vec<u8>,
) -> Vec<u8> {
    let mut out = format!("{name} {{ ").into_bytes();
    for (i, (f, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(f.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&one(v));
    }
    out.extend_from_slice(b" }");
    out
}

fn display_items(items: &[Value], mut one: impl FnMut(&Value) -> Vec<u8>) -> Vec<u8> {
    let mut out = b"[".to_vec();
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(&one(v));
    }
    out.extend_from_slice(b"]");
    out
}

fn render_struct(
    name: &str,
    fields: &[(String, Value)],
    mut one: impl FnMut(&Value) -> String,
) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(f, v)| format!("{f}: {}", one(v)))
        .collect();
    format!("{name} {{ {} }}", fields.join(", "))
}

fn render_items(items: &[Value], one: impl FnMut(&Value) -> String) -> String {
    let items: Vec<String> = items.iter().map(one).collect();
    format!("[{}]", items.join(", "))
}
