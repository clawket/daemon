use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::Unit;
use crate::repo::units;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/units", get(list).post(create))
        .route(
            "/units/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/units/{id}/approve", post(approve))
}

#[derive(Deserialize)]
struct ListQuery {
    plan_id: Option<String>,
    status: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Unit>>> {
    Ok(Json(units::list(
        &app.conn(),
        units::ListFilter {
            plan_id: q.plan_id.as_deref(),
            status: q.status.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    plan_id: String,
    title: String,
    goal: Option<String>,
    idx: Option<i64>,
    approval_required: Option<bool>,
    execution_mode: Option<String>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Unit>> {
    json_or_404(units::create(
        &app.conn(),
        units::CreateInput {
            plan_id: &body.plan_id,
            title: &body.title,
            goal: body.goal.as_deref(),
            idx: body.idx,
            approval_required: body.approval_required.unwrap_or(false),
            execution_mode: body.execution_mode.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Unit>> {
    json_or_404(units::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    units::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize, Default)]
struct ApproveBody {
    by: Option<String>,
}

async fn approve(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ApproveBody>>,
) -> ApiResult<Json<Unit>> {
    let by = body.and_then(|b| b.0.by).unwrap_or_else(|| "human".into());
    let result = units::approve(&app.conn(), &id, &by)?;
    if result.is_some() {
        app.emit("unit:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Unit>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = units::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("goal") {
        f.goal = Some(v.as_str().map(String::from));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        f.status = Some(s.into());
    }
    if let Some(s) = obj.get("execution_mode").and_then(Value::as_str) {
        f.execution_mode = Some(s.into());
    }
    let result = units::update(&app.conn(), &id, f)?;
    if result.is_some() {
        app.emit("unit:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}
