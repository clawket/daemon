use axum::extract::{Query, State};
use axum::extract::{Path};
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::TaskComment;
use crate::repo::{comments, tasks};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{id}/comments", get(list).post(create))
        // FIX-DAEMON-103: collection-level routes for CLI compatibility (CLI
        // POSTs to `/comments` with task_id in body; was 404 before this).
        .route("/comments", get(list_collection).post(create_collection))
        // FIX-DAEMON-111: PATCH to update body, DELETE now soft-deletes
        .route("/comments/{id}", patch(update_one).delete(delete_one))
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

#[derive(Deserialize)]
struct CreateCollectionBody {
    author: String,
    body: String,
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    #[allow(dead_code)]
    label: Option<String>,
}

async fn create_collection(
    State(app): State<AppState>,
    Json(payload): Json<CreateCollectionBody>,
) -> ApiResult<Json<TaskComment>> {
    if payload.unit_id.is_some() || payload.plan_id.is_some() {
        return Err(ApiError::bad_request(
            "unit/plan comments not supported (schema only has task_comments)",
        ));
    }
    let task_id = payload
        .task_id
        .ok_or_else(|| ApiError::bad_request("task_id is required"))?;
    let conn = app.conn();
    let task = tasks::get(&conn, &task_id)?
        .ok_or_else(|| ApiError::not_found("Task not found"))?;
    json_or_404(comments::create(&conn, &task.id, &payload.author, &payload.body)?)
}

#[derive(Deserialize)]
struct ListCollectionQuery {
    task_id: Option<String>,
}

async fn list_collection(
    State(app): State<AppState>,
    Query(q): Query<ListCollectionQuery>,
) -> ApiResult<Json<Vec<TaskComment>>> {
    let task_id = q
        .task_id
        .ok_or_else(|| ApiError::bad_request("task_id query param is required"))?;
    let conn = app.conn();
    let task = tasks::get(&conn, &task_id)?
        .ok_or_else(|| ApiError::not_found("Task not found"))?;
    Ok(Json(comments::list(&conn, &task.id)?))
}

/// FIX-DAEMON-111: PATCH /comments/:id — update comment body
#[derive(Deserialize)]
struct UpdateBody {
    body: String,
}

async fn update_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateBody>,
) -> ApiResult<Json<TaskComment>> {
    json_or_404(comments::update_body(&app.conn(), &id, &payload.body)?)
}

/// FIX-DAEMON-111: DELETE /comments/:id — soft-delete (prefixes body with [DELETED])
async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    comments::soft_delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "soft_deleted": id })))
}
