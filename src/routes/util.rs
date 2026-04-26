// Shared helpers for route handlers.
//
// `norm_opt` collapses whitespace-only / empty strings to None. Node v2.2.1
// uses `value || null` at the route boundary, which turns `""` into `null`;
// without an equivalent pass here, empty strings slip through Axum's
// deserialization and hit SQLite as `''`, producing FK violations or orphan
// rows for nullable foreign keys.

use serde_json::Value;

pub fn norm_opt(s: Option<String>) -> Option<String> {
    s.and_then(|v| {
        if v.trim().is_empty() {
            None
        } else {
            Some(v)
        }
    })
}

pub fn value_to_opt_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}
