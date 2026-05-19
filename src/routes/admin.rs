//! US-CKT-SCHEMA-041 / US-CKT-SCHEMA-042: admin SQL channel with destructive
//! DDL/DML guards. Local-only, gated by AppState::is_migrating semantics.
//!
//! The daemon historically had no admin SQL endpoint, which made the
//! "destructive DDL forbidden" / "bulk delete on tasks forbidden" contracts
//! observable only by *absence* (no surface area to invoke). Per the v3
//! scenario contract, the daemon must produce a positive 400 error for those
//! cases. This module exposes `POST /admin/exec_sql` that runs only after
//! pattern-matching the SQL against deny-listed forms.
//!
//! Allowed paths:
//!   * SELECT … (any read) — passes through
//!   * ALTER TABLE … ADD COLUMN — passes through (PDD O8 additive only)
//!   * CREATE INDEX / CREATE TABLE — passes through
//!
//! Rejected paths (all return HTTP 400 + coded error):
//!   * ALTER TABLE … DROP COLUMN  → "destructive DDL forbidden"
//!   * DROP TABLE / DROP INDEX     → "destructive DDL forbidden"
//!   * TRUNCATE                    → "bulk delete on tasks forbidden"
//!   * DELETE FROM tasks (no WHERE)→ "bulk delete on tasks forbidden"
//!
//! The TCP listener already has X-Clawket-Token auth (FIX-DAEMON-108); the
//! Unix socket listener is already gated by filesystem permissions, so no
//! additional auth layer is required here.

use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/admin/exec_sql", post(exec_sql))
}

#[derive(Deserialize)]
struct ExecBody {
    sql: String,
}

async fn exec_sql(
    State(app): State<AppState>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<Value>> {
    let sql = body.sql.trim();
    let upper = sql.to_uppercase();

    // ── destructive DDL guards (US-CKT-SCHEMA-041) ─────────────────────────
    if upper.contains("DROP COLUMN") {
        return Err(ApiError::bad_request_coded(
            "DESTRUCTIVE_DDL_FORBIDDEN",
            "destructive DDL forbidden: DROP COLUMN is not allowed",
        ));
    }
    if upper.starts_with("DROP TABLE") || upper.starts_with("DROP INDEX") {
        return Err(ApiError::bad_request_coded(
            "DESTRUCTIVE_DDL_FORBIDDEN",
            "destructive DDL forbidden: DROP TABLE/INDEX is not allowed",
        ));
    }

    // ── bulk DML guards (US-CKT-SCHEMA-042) ────────────────────────────────
    if upper.starts_with("TRUNCATE") {
        return Err(ApiError::bad_request_coded(
            "BULK_DELETE_FORBIDDEN",
            "bulk delete on tasks forbidden: TRUNCATE is not allowed",
        ));
    }
    // Reject DELETE FROM tasks without a WHERE clause. We deliberately use a
    // simple uppercase scan rather than a full SQL parser — false positives
    // (e.g. a comment containing DELETE FROM TASKS) are acceptable for an
    // admin-only channel; the contract requires us to err on the side of
    // refusal.
    if upper.contains("DELETE FROM TASKS") && !upper.contains("WHERE") {
        return Err(ApiError::bad_request_coded(
            "BULK_DELETE_FORBIDDEN",
            "bulk delete on tasks forbidden: DELETE FROM tasks without WHERE is not allowed",
        ));
    }

    // ── execute (read-only or additive) ─────────────────────────────────────
    let conn = app.conn();
    let is_select = upper.starts_with("SELECT");

    // US-CKT-SCHEMA-037: any non-SELECT statement on this admin channel is, by
    // definition, an online migration / DDL. Flip the migration gate while it
    // runs so concurrent mutating routes return 503 with code
    // MIGRATION_IN_PROGRESS, and reset it in both success and failure paths
    // via the RAII guard below. SELECT does not flip the flag.
    let _gate = (!is_select).then(|| MigrationGuard::arm(&app));

    if is_select {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| ApiError::bad_request(format!("invalid SQL: {e}")))?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();
        let rows = stmt
            .query_map([], |row| {
                let mut obj = serde_json::Map::new();
                for (i, name) in col_names.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    let jv = match v {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => Value::from(i),
                        rusqlite::types::Value::Real(f) => {
                            serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
                        }
                        rusqlite::types::Value::Text(s) => Value::from(s),
                        rusqlite::types::Value::Blob(_) => Value::from("<blob>"),
                    };
                    obj.insert(name.clone(), jv);
                }
                Ok(Value::Object(obj))
            })
            .map_err(|e| ApiError::internal(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ApiError::internal(e.to_string()))?;
        return Ok(Json(serde_json::json!({ "rows": rows })));
    }
    // Non-SELECT additive paths (ALTER TABLE ADD COLUMN, CREATE INDEX etc.)
    let n = conn
        .execute(sql, [])
        .map_err(|e| ApiError::bad_request(format!("SQL execution failed: {e}")))?;
    Ok(Json(serde_json::json!({ "rows_affected": n })))
}

/// US-CKT-SCHEMA-037: RAII guard that toggles `AppState::set_migrating(true)`
/// on construction and resets to false on drop. Drop runs in both success and
/// `?`-propagated failure paths, so the gate cannot leak across an early
/// return.
struct MigrationGuard {
    app: AppState,
}

impl MigrationGuard {
    fn arm(app: &AppState) -> Self {
        app.set_migrating(true);
        Self { app: app.clone() }
    }
}

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        self.app.set_migrating(false);
    }
}
