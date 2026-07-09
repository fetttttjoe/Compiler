//! Turning values into text: `display` is what `print` writes (scalars
//! and strings raw, aggregates source-like), `render` the debug-style
//! result line. Struct fields render in name-sorted storage order —
//! observable language spec pinned by the conformance corpus, so a
//! future compiled renderer must sort the same way. Both are
//! depth-capped: cyclic refstructs would recurse forever otherwise.

use super::*;

impl Value {
    /// Human-facing rendering for `print`: scalars and strings raw, structs
    /// and arrays in source-like shape, refs printed through the handle.
    /// Depth-capped like `render` — a handle hop costs a level, so cycles
    /// stay as bounded as they were under the Rc oracle.
    pub fn display(&self, heap: &Heap) -> String {
        self.display_depth(heap, 8)
    }

    fn display_depth(&self, heap: &Heap, depth: usize) -> String {
        if depth == 0 {
            return "...".to_string();
        }
        match self {
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s.clone(),
            Value::Null => "null".to_string(),
            Value::Unit => "unit".to_string(),
            // The hop consumes a level, the object's children another.
            Value::Ref(id) if depth == 1 => {
                let _ = id;
                "...".to_string()
            }
            Value::Ref(id) => {
                let obj = &heap.structs[*id];
                render_struct(&obj.name, &obj.fields, |v| v.display_depth(heap, depth - 2))
            }
            Value::Array(id) => {
                render_items(&heap.arrays[*id], |v| v.display_depth(heap, depth - 1))
            }
            Value::Struct { name, fields } => {
                render_struct(name, fields, |v| v.display_depth(heap, depth - 1))
            }
        }
    }

    /// Debug-style rendering with a depth cap — cyclic refstruct values
    /// would recurse forever otherwise.
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
