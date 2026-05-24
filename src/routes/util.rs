// Shared helpers for route handlers.
//
// `norm_opt` collapses whitespace-only / empty strings to None. Node v2.2.1
// uses `value || null` at the route boundary, which turns `""` into `null`;
// without an equivalent pass here, empty strings slip through Axum's
// deserialization and hit SQLite as `''`, producing FK violations or orphan
// rows for nullable foreign keys.

use rusqlite::Connection;
use serde_json::Value;

use crate::repo::projects;
use crate::routes::error::ApiError;

pub fn norm_opt(s: Option<String>) -> Option<String> {
    s.and_then(|v| if v.trim().is_empty() { None } else { Some(v) })
}

pub fn value_to_opt_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Resolve a user-supplied project reference (canonical id or short ticket
/// key) to the canonical `PROJ-<slug>` id. Returns a structured 404
/// `PROJECT_NOT_FOUND` error when neither form matches an existing project.
///
/// Use at every boundary where `project_id` arrives from the user (request
/// body, query string, path segment). Previously the value was passed
/// verbatim into INSERTs, which made `--project SDI` (the visible ticket
/// key) fail with an opaque "insert plan" FK violation instead of a clear
/// "project 'SDI' not found" message.
pub fn resolve_project_ref(conn: &Connection, value: &str) -> Result<String, ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::bad_request_coded(
            "MISSING_PROJECT",
            "MISSING_PROJECT: project reference is required",
        ));
    }
    projects::get_by_ref(conn, value)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map(|p| p.id)
        .ok_or_else(|| {
            ApiError::not_found_coded(
                "PROJECT_NOT_FOUND",
                format!(
                    "PROJECT_NOT_FOUND: no project matches '{}' (looked up by id and by ticket key)",
                    value
                ),
            )
        })
}

/// Same as [`resolve_project_ref`] but accepts an `Option<&str>`. Returns
/// `Ok(None)` when the input is `None` so callers using filters (list/query
/// endpoints) can pass through "no filter" without an error.
pub fn resolve_project_ref_opt(
    conn: &Connection,
    value: Option<&str>,
) -> Result<Option<String>, ApiError> {
    match value {
        Some(v) if !v.trim().is_empty() => resolve_project_ref(conn, v).map(Some),
        _ => Ok(None),
    }
}
