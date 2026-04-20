use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::params;
use serde::Deserialize;

use crate::models::Task;
use crate::repo::tasks;
use crate::routes::error::{json_or_404, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{id}/labels", post(add))
        .route("/tasks/{id}/labels/{label}", axum::routing::delete(remove))
        .route("/labels/{label}/tasks", get(by_label))
}

#[derive(Deserialize)]
struct AddBody {
    label: String,
}

async fn add(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddBody>,
) -> ApiResult<Json<Task>> {
    json_or_404(tasks::add_label(&app.conn(), &id, &body.label)?)
}

async fn remove(
    State(app): State<AppState>,
    Path((id, label)): Path<(String, String)>,
) -> ApiResult<Json<Task>> {
    json_or_404(tasks::remove_label(&app.conn(), &id, &label)?)
}

async fn by_label(
    State(app): State<AppState>,
    Path(label): Path<String>,
) -> ApiResult<Json<Vec<Task>>> {
    let conn = app.conn();
    let mut stmt = conn.prepare("SELECT task_id FROM task_labels WHERE label = ?1")?;
    let ids: Vec<String> = stmt
        .query_map(params![label], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    let mut out = Vec::new();
    for id in ids {
        if let Some(t) = tasks::get(&conn, &id)? {
            out.push(t);
        }
    }
    Ok(Json(out))
}
