use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::TaskComment;
use crate::repo::{comments, tasks};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{id}/comments", get(list).post(create))
        .route("/comments/{id}", delete(delete_one))
}

async fn list(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<TaskComment>>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("Task not found"))?;
    Ok(Json(comments::list(&conn, &task.id)?))
}

#[derive(Deserialize)]
struct CreateBody {
    author: String,
    body: String,
}

async fn create(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<TaskComment>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("Task not found"))?;
    json_or_404(comments::create(&conn, &task.id, &body.author, &body.body)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    comments::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}
