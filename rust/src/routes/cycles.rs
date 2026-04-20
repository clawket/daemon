use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::{Cycle, Task};
use crate::repo::{cycles, tasks};
use crate::routes::error::{json_or_404, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cycles", get(list).post(create))
        .route("/cycles/{id}", get(get_one).delete(delete_one))
        .route("/cycles/{id}/activate", post(activate))
        .route("/cycles/{id}/complete", post(complete))
        .route("/cycles/{id}/tasks", get(list_tasks))
}

#[derive(Deserialize)]
struct ListQuery {
    project_id: Option<String>,
    status: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Cycle>>> {
    Ok(Json(cycles::list(
        &app.conn(),
        cycles::ListFilter {
            project_id: q.project_id.as_deref(),
            status: q.status.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    project_id: String,
    title: String,
    goal: Option<String>,
    idx: Option<i64>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Cycle>> {
    json_or_404(cycles::create(
        &app.conn(),
        cycles::CreateInput {
            project_id: &body.project_id,
            title: &body.title,
            goal: body.goal.as_deref(),
            idx: body.idx,
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Cycle>> {
    json_or_404(cycles::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    cycles::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn activate(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Cycle>> {
    json_or_404(cycles::activate(&app.conn(), &id)?)
}

async fn complete(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Cycle>> {
    json_or_404(cycles::complete(&app.conn(), &id)?)
}

async fn list_tasks(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Task>>> {
    Ok(Json(tasks::list(
        &app.conn(),
        tasks::ListFilter {
            cycle_id: Some(&id),
            ..Default::default()
        },
    )?))
}
