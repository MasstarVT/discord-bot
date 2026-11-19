//! Port of `src/utils/errorFormatter.js`: normalises an error into a plain object for structured logging.

use serde_json::{Map, Value};

/// `formatError(err, context)` for a Rust error: `{ message, name, stack, code, ...context }`. `name` is the error's
/// type name, `stack` is its source chain (Rust has no stack trace to give), `code` is null. Context keys are spread
/// last, so a context key overrides a field of the same name.
pub fn format_error(err: &(dyn std::error::Error + 'static), context: &Map<String, Value>) -> Map<String, Value> {
    let mut chain = vec![err.to_string()];
    let mut src = err.source();
    while let Some(s) = src {
        chain.push(format!("Caused by: {s}"));
        src = s.source();
    }
    let name = std::any::type_name_of_val(err).rsplit("::").next().unwrap_or("Error").to_string();
    let mut out = Map::new();
    out.insert("message".into(), Value::String(err.to_string()));
    out.insert("name".into(), Value::String(name));
    out.insert("stack".into(), Value::String(chain.join("\n")));
    out.insert("code".into(), Value::Null);
    for (k, v) in context {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// `formatError` for an error-like value with an explicit name and code (a JS `Error` crossing into Rust).
pub fn format_error_parts(message: &str, name: &str, stack: Option<&str>, code: Option<Value>, context: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("message".into(), Value::String(message.into()));
    out.insert("name".into(), Value::String(name.into()));
    out.insert("stack".into(), stack.map_or(Value::Null, |s| Value::String(s.into())));
    out.insert("code".into(), code.unwrap_or(Value::Null));
    for (k, v) in context {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// `formatError` for a thrown value that is not an Error: `{ message: String(err), ...context }`.
pub fn format_non_error(message: &str, context: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("message".into(), Value::String(message.into()));
    for (k, v) in context {
        out.insert(k.clone(), v.clone());
    }
    out
}
