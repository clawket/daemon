use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::Project;
use crate::repo::projects;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/projects", get(list).post(create))
        .route(
            "/projects/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/projects/{id}/cwds", post(add_cwd).delete(remove_cwd))
        .route("/projects/by-cwd/{*cwd}", get(by_cwd))
}

async fn list(State(app): State<AppState>) -> ApiResult<Json<Vec<Project>>> {
    let conn = app.conn();
    Ok(Json(projects::list(&conn)?))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    description: Option<String>,
    cwd: Option<String>,
    key: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Project>> {
    let mut conn = app.conn();
    let project = projects::create(
        &mut conn,
        projects::CreateInput {
            name: &body.name,
            description: body.description.as_deref(),
            cwd: body.cwd.as_deref(),
            key: body.key.as_deref(),
        },
    )?;
    json_or_404(project)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Project>> {
    json_or_404(projects::get(&app.conn(), &id)?)
}

async fn delete_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    projects::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize)]
struct CwdBody {
    cwd: String,
}

async fn add_cwd(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CwdBody>,
) -> ApiResult<Json<Project>> {
    json_or_404(projects::add_cwd(&app.conn(), &id, &body.cwd)?)
}

async fn remove_cwd(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CwdBody>,
) -> ApiResult<Json<Project>> {
    json_or_404(projects::remove_cwd(&app.conn(), &id, &body.cwd)?)
}

#[derive(Deserialize)]
struct ByCwdQuery {
    enabled_only: Option<bool>,
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Project>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = projects::UpdateFields::default();
    if let Some(s) = obj.get("name").and_then(Value::as_str) {
        f.name = Some(s.into());
    }
    if let Some(v) = obj.get("description") {
        f.description = Some(v.as_str().map(String::from));
    }
    if let Some(v) = obj.get("key") {
        f.key = Some(v.as_str().map(String::from));
    }
    if let Some(v) = obj.get("enabled") {
        let n = if let Some(b) = v.as_bool() {
            if b { 1 } else { 0 }
        } else if let Some(i) = v.as_i64() {
            i
        } else {
            return Err(ApiError::bad_request("enabled must be bool or int"));
        };
        f.enabled = Some(n);
    }
    if let Some(arr) = obj.get("wiki_paths").and_then(Value::as_array) {
        let paths: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        f.wiki_paths = Some(paths);
    }
    json_or_404(projects::update(&app.conn(), &id, f)?)
}

async fn by_cwd(
    State(app): State<AppState>,
    Path(cwd): Path<String>,
    Query(q): Query<ByCwdQuery>,
) -> ApiResult<Json<Project>> {
    let prefixed = if cwd.starts_with('/') {
        cwd
    } else {
        format!("/{}", cwd)
    };
    json_or_404(projects::get_by_cwd(
        &app.conn(),
        &prefixed,
        q.enabled_only.unwrap_or(false),
    )?)
}

