use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::Run;
use crate::repo::runs;
use crate::routes::error::{json_or_404, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list).post(create))
        .route("/runs/{id}", get(get_one))
        .route("/runs/{id}/finish", post(finish))
}

#[derive(Deserialize)]
struct ListQuery {
    task_id: Option<String>,
    session_id: Option<String>,
    project_id: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Run>>> {
    Ok(Json(runs::list(
        &app.conn(),
        runs::ListFilter {
            task_id: q.task_id.as_deref(),
            session_id: q.session_id.as_deref(),
            project_id: q.project_id.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    task_id: String,
    session_id: Option<String>,
    agent: Option<String>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Run>> {
    json_or_404(runs::create(
        &app.conn(),
        &body.task_id,
        body.session_id.as_deref(),
        body.agent.as_deref().unwrap_or("main"),
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Run>> {
    json_or_404(runs::get(&app.conn(), &id)?)
}

#[derive(Deserialize)]
struct FinishBody {
    result: String,
    notes: Option<String>,
}

async fn finish(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FinishBody>,
) -> ApiResult<Json<Run>> {
    json_or_404(runs::finish(
        &app.conn(),
        &id,
        &body.result,
        body.notes.as_deref(),
    )?)
}
