use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::TaskRelation;
use crate::repo::task_relations;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{id}/relations", get(list).post(create))
        .route("/relations/{id}", axum::routing::delete(delete_one))
}

async fn list(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<TaskRelation>>> {
    Ok(Json(task_relations::list(
        &app.conn(),
        task_relations::ListFilter {
            task_id: Some(&id),
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
    Ok(Json(task_relations::create(
        &app.conn(),
        &id,
        &body.target_task_id,
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
