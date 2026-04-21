use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::Plan;
use crate::repo::plans;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::{norm_opt, value_to_opt_string};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans", get(list).post(create))
        .route(
            "/plans/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
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
    let description = norm_opt(body.description);
    let source = norm_opt(body.source);
    let source_path = norm_opt(body.source_path);
    json_or_404(plans::create(
        &app.conn(),
        plans::CreateInput {
            project_id: &body.project_id,
            title: &body.title,
            description: description.as_deref(),
            source: source.as_deref(),
            source_path: source_path.as_deref(),
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
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

async fn approve(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Plan>> {
    let result = plans::approve(&app.conn(), &id)?;
    if result.is_some() {
        app.emit("plan:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Plan>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = plans::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("description") {
        f.description = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        // Block draft→active: require /plans/:id/approve to capture approval metadata.
        if s == "active" {
            if let Some(existing) = plans::get(&app.conn(), &id)? {
                if existing.status == "draft" {
                    return Err(ApiError::bad_request(
                        "Use POST /plans/:id/approve to activate a draft plan",
                    ));
                }
            }
        }
        f.status = Some(s.into());
    }
    if let Some(v) = obj.get("approved_at") {
        f.approved_at = Some(v.as_i64());
    }
    let result = plans::update(&app.conn(), &id, f)?;
    if result.is_some() {
        app.emit("plan:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}
