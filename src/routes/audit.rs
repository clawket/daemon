// FIX-DAEMON-107: /audit route — read-only view of the audit_log table.
//
// GET  /audit        — list entries (filtered by entity_type / entity_id / actor / op_type / limit)
// POST /audit        → 405 METHOD_NOT_ALLOWED (audit log is immutable via API)
// PUT  /audit        → 405
// DELETE /audit      → 405
//
// The audit_log table is written exclusively by the Rust layer (inside
// mutations). External callers cannot inject or modify entries.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::AuditLogEntry;
use crate::repo::audit_log;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        // GET is the only allowed method
        .route("/audit", get(list))
        // All mutating methods → 405 (audit log is immutable)
        .route("/audit", post(method_not_allowed))
        .route("/audit", put(method_not_allowed))
        .route("/audit", delete(method_not_allowed))
        .route("/audit", patch(method_not_allowed))
}

#[derive(Deserialize)]
struct ListQuery {
    entity_type: Option<String>,
    entity_id: Option<String>,
    actor: Option<String>,
    op_type: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<AuditLogEntry>>> {
    Ok(Json(audit_log::list(
        &app.conn(),
        audit_log::ListFilter {
            entity_type: q.entity_type.as_deref(),
            entity_id: q.entity_id.as_deref(),
            actor: q.actor.as_deref(),
            op_type: q.op_type.as_deref(),
            limit: q.limit,
        },
    )?))
}

/// FIX-DAEMON-107: 405 METHOD_NOT_ALLOWED — audit log is append-only; no
/// external writes, updates, or deletes permitted.
async fn method_not_allowed() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({
            "error": "audit log is immutable; POST/PUT/DELETE/PATCH are not allowed",
            "code": "AUDIT_IMMUTABLE"
        })),
    )
}
