use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::Plan;
use crate::repo::plans;
use crate::routes::error::{json_or_404, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans", get(list).post(create))
        .route("/plans/{id}", get(get_one).delete(delete_one))
        .route("/plans/{id}/approve", post(approve))
}

#[derive(Deserialize)]
struct ListQuery {
    project_id: Option<String>,
    status: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Plan>>> {
    Ok(Json(plans::list(
        &app.conn(),
        plans::ListFilter {
            project_id: q.project_id.as_deref(),
            status: q.status.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    project_id: String,
    title: String,
    description: Option<String>,
    source: Option<String>,
    source_path: Option<String>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Plan>> {
    json_or_404(plans::create(
        &app.conn(),
        plans::CreateInput {
            project_id: &body.project_id,
            title: &body.title,
            description: body.description.as_deref(),
            source: body.source.as_deref(),
            source_path: body.source_path.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Plan>> {
    json_or_404(plans::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    plans::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn approve(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Plan>> {
    json_or_404(plans::approve(&app.conn(), &id)?)
}
