use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::{Artifact, ArtifactVersion};
use crate::repo::artifacts;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/artifacts", get(list).post(create))
        .route(
            "/artifacts/{id}",
            get(get_one).patch(update).delete(delete_one),
        )
        .route("/artifacts/{id}/versions", get(list_versions))
}

#[derive(Deserialize)]
struct ListQuery {
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    scope: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Artifact>>> {
    Ok(Json(artifacts::list(
        &app.conn(),
        artifacts::ListFilter {
            task_id: q.task_id.as_deref(),
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            type_: q.type_.as_deref(),
            scope: q.scope.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    task_id: Option<String>,
    unit_id: Option<String>,
    plan_id: Option<String>,
    #[serde(rename = "type")]
    type_: String,
    title: String,
    content: Option<String>,
    content_format: Option<String>,
    parent_id: Option<String>,
    scope: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Artifact>> {
    json_or_404(artifacts::create(
        &app.conn(),
        artifacts::CreateInput {
            task_id: body.task_id.as_deref(),
            unit_id: body.unit_id.as_deref(),
            plan_id: body.plan_id.as_deref(),
            type_: &body.type_,
            title: &body.title,
            content: body.content.as_deref(),
            content_format: body.content_format.as_deref(),
            parent_id: body.parent_id.as_deref(),
            scope: body.scope.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Artifact>> {
    json_or_404(artifacts::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    artifacts::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct UpdateBody {
    title: Option<String>,
    content: Option<String>,
    content_format: Option<String>,
    scope: Option<String>,
    created_by: Option<String>,
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<Artifact>> {
    let f = artifacts::UpdateFields {
        title: body.title,
        content: body.content,
        content_format: body.content_format,
        scope: body.scope,
        created_by: body.created_by,
    };
    artifacts::update(&app.conn(), &id, f)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("artifact not found"))
}

async fn list_versions(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ArtifactVersion>>> {
    Ok(Json(artifacts::list_versions(&app.conn(), &id)?))
}
