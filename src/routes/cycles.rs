use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::{Cycle, Task};
use crate::repo::{cycles, tasks};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::{norm_opt, value_to_opt_string};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cycles", get(list).post(create))
        .route(
            "/cycles/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
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
    let goal = norm_opt(body.goal);
    json_or_404(cycles::create(
        &app.conn(),
        cycles::CreateInput {
            project_id: &body.project_id,
            title: &body.title,
            goal: goal.as_deref(),
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
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

async fn activate(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Cycle>> {
    let result = cycles::activate(&app.conn(), &id)?;
    if result.is_some() {
        app.emit("cycle:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

async fn complete(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Cycle>> {
    let result = cycles::complete(&app.conn(), &id)?;
    if result.is_some() {
        app.emit("cycle:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Cycle>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = cycles::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("goal") {
        f.goal = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        // Block planning→active: require /cycles/:id/activate so started_at is set.
        if s == "active" {
            if let Some(existing) = cycles::get(&app.conn(), &id)? {
                if existing.status == "planning" {
                    return Err(ApiError::bad_request(
                        "Use POST /cycles/:id/activate to start a planning cycle",
                    ));
                }
            }
        }
        f.status = Some(s.into());
    }
    let result = cycles::update(&app.conn(), &id, f)?;
    if result.is_some() {
        app.emit("cycle:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
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
