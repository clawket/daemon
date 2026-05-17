use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::TaskRelation;
use crate::repo::{task_relations, tasks};
use crate::routes::error::{localize_error, ApiError, ApiResult};
use crate::state::AppState;
// I18N-040: route-level locale resolver shim. Reads CLAWKET_LOCALE / LC_ALL / LANG.
// TODO(v3-r3): complete propagation — accept Accept-Language / X-Clawket-Locale headers
// at the axum extractor layer and thread per-request locale into every handler.
fn current_locale() -> String {
    crate::locale::resolve_locale()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{id}/relations", get(list).post(create))
        .route("/relations/{id}", axum::routing::delete(delete_one))
}

async fn list(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<TaskRelation>>> {
    let conn = app.conn();
    let lang = current_locale();
    // US-CLAWKET-I18N-040: localize TASK_NOT_FOUND uniformly across relations.
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(localize_error("TASK_NOT_FOUND", &lang)))?;
    Ok(Json(task_relations::list(
        &conn,
        task_relations::ListFilter {
            task_id: Some(&task.id),
            ..Default::default()
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    target_task_id: String,
    relation_type: String,
}

async fn create(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<TaskRelation>> {
    let conn = app.conn();
    let lang = current_locale();
    let source = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(localize_error("TASK_NOT_FOUND", &lang)))?;
    let target = tasks::get(&conn, &body.target_task_id)?
        .ok_or_else(|| ApiError::not_found(localize_error("TASK_NOT_FOUND", &lang)))?;
    Ok(Json(task_relations::create(
        &conn,
        &source.id,
        &target.id,
        &body.relation_type,
    )?))
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    task_relations::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}
