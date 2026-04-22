use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::Question;
use crate::repo::questions;
use crate::routes::error::{json_or_404, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/questions", get(list).post(create))
        .route("/questions/{id}", get(get_one))
        .route("/questions/{id}/answer", post(answer))
}

#[derive(Deserialize)]
struct ListQuery {
    plan_id: Option<String>,
    unit_id: Option<String>,
    task_id: Option<String>,
    pending: Option<bool>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Question>>> {
    Ok(Json(questions::list(
        &app.conn(),
        questions::ListFilter {
            plan_id: q.plan_id.as_deref(),
            unit_id: q.unit_id.as_deref(),
            task_id: q.task_id.as_deref(),
            pending: q.pending,
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    plan_id: Option<String>,
    unit_id: Option<String>,
    task_id: Option<String>,
    kind: Option<String>,
    origin: Option<String>,
    body: String,
    asked_by: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Question>> {
    json_or_404(questions::create(
        &app.conn(),
        questions::CreateInput {
            plan_id: body.plan_id.as_deref(),
            unit_id: body.unit_id.as_deref(),
            task_id: body.task_id.as_deref(),
            kind: body.kind.as_deref().unwrap_or("clarification"),
            origin: body.origin.as_deref().unwrap_or("prompt"),
            body: &body.body,
            asked_by: body.asked_by.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Question>> {
    json_or_404(questions::get(&app.conn(), &id)?)
}

#[derive(Deserialize)]
struct AnswerBody {
    answer: String,
    answered_by: Option<String>,
}

async fn answer(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AnswerBody>,
) -> ApiResult<Json<Question>> {
    json_or_404(questions::answer(
        &app.conn(),
        &id,
        &body.answer,
        body.answered_by.as_deref(),
    )?)
}
