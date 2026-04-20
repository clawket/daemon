use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::params;
use serde::Deserialize;

use crate::models::Task;
use crate::repo::tasks;
use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/backlog", get(list))
}

#[derive(Deserialize)]
struct BacklogQuery {
    project_id: String,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<BacklogQuery>,
) -> ApiResult<Json<Vec<Task>>> {
    let conn = app.conn();
    let mut stmt = conn.prepare(
        "SELECT s.id FROM tasks s
         JOIN units u ON u.id = s.unit_id
         JOIN plans pl ON pl.id = u.plan_id
         WHERE pl.project_id = ?1 AND s.cycle_id IS NULL
         ORDER BY s.created_at",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![q.project_id], |r| r.get::<_, String>(0))
        .map_err(|e| ApiError::internal(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    let mut out = Vec::new();
    for id in ids {
        if let Some(t) = tasks::get(&conn, &id)? {
            out.push(t);
        }
    }
    Ok(Json(out))
}
