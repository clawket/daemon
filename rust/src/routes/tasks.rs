use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::Task;
use crate::repo::tasks;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(create))
        .route(
            "/tasks/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/tasks/{id}/body", post(append_body))
        .route("/tasks/bulk-update", post(bulk_update))
}

#[derive(Deserialize)]
struct ListQuery {
    unit_id: Option<String>,
    plan_id: Option<String>,
    status: Option<String>,
    cycle_id: Option<String>,
    assignee: Option<String>,
    agent_id: Option<String>,
    parent_task_id: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Task>>> {
    let parent = q
        .parent_task_id
        .as_deref()
        .map(|s| if s == "null" { None } else { Some(s) });
    Ok(Json(tasks::list(
        &app.conn(),
        tasks::ListFilter {
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            status: q.status.as_deref(),
            cycle_id: q.cycle_id.as_deref(),
            assignee: q.assignee.as_deref(),
            agent_id: q.agent_id.as_deref(),
            parent_task_id: parent,
        },
    )?))
}

#[derive(Deserialize, Default)]
struct CreateBody {
    unit_id: String,
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    idx: Option<i64>,
    #[serde(default)]
    depends_on: Vec<String>,
    parent_task_id: Option<String>,
    priority: Option<String>,
    complexity: Option<String>,
    estimated_edits: Option<i64>,
    cycle_id: Option<String>,
    reporter: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Task>> {
    let mut conn = app.conn();
    json_or_404(tasks::create(
        &mut conn,
        tasks::CreateInput {
            unit_id: &body.unit_id,
            title: &body.title,
            body: body.body.as_deref(),
            assignee: body.assignee.as_deref(),
            idx: body.idx,
            depends_on: body.depends_on,
            parent_task_id: body.parent_task_id.as_deref(),
            priority: body.priority.as_deref(),
            complexity: body.complexity.as_deref(),
            estimated_edits: body.estimated_edits,
            cycle_id: body.cycle_id.as_deref(),
            reporter: body.reporter.as_deref(),
            type_: body.type_.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Task>> {
    json_or_404(tasks::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    tasks::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct AppendBody {
    text: String,
}

async fn append_body(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AppendBody>,
) -> ApiResult<Json<Task>> {
    json_or_404(tasks::append_body(&app.conn(), &id, &body.text)?)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Task>> {
    let mut conn = app.conn();
    json_or_404(tasks::update(&mut conn, &id, parse_update(&body)?)?)
}

#[derive(Deserialize)]
struct BulkBody {
    ids: Vec<String>,
    fields: Value,
}

async fn bulk_update(
    State(app): State<AppState>,
    Json(body): Json<BulkBody>,
) -> ApiResult<Json<Vec<Task>>> {
    let mut conn = app.conn();
    let mut out = Vec::new();
    for id in &body.ids {
        if let Some(t) = tasks::update(&mut conn, id, parse_update(&body.fields)?)? {
            out.push(t);
        }
    }
    Ok(Json(out))
}

fn parse_update(v: &Value) -> ApiResult<tasks::UpdateFields> {
    let obj = v
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = tasks::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("body") {
        f.body = Some(v.as_str().map(String::from));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        f.status = Some(s.into());
    }
    if let Some(v) = obj.get("assignee") {
        f.assignee = Some(v.as_str().map(String::from));
    }
    if let Some(s) = obj.get("priority").and_then(Value::as_str) {
        f.priority = Some(s.into());
    }
    if let Some(v) = obj.get("complexity") {
        f.complexity = Some(v.as_str().map(String::from));
    }
    if let Some(v) = obj.get("estimated_edits") {
        f.estimated_edits = Some(v.as_i64());
    }
    if let Some(v) = obj.get("parent_task_id") {
        f.parent_task_id = Some(v.as_str().map(String::from));
    }
    if let Some(v) = obj.get("cycle_id") {
        f.cycle_id = Some(v.as_str().map(String::from));
    }
    if let Some(s) = obj.get("unit_id").and_then(Value::as_str) {
        f.unit_id = Some(s.into());
    }
    if let Some(v) = obj.get("reporter") {
        f.reporter = Some(v.as_str().map(String::from));
    }
    if let Some(s) = obj.get("type").and_then(Value::as_str) {
        f.type_ = Some(s.into());
    }
    if let Some(v) = obj.get("agent_id") {
        f.agent_id = Some(v.as_str().map(String::from));
    }
    Ok(f)
}
