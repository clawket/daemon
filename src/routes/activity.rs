use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::models::ActivityLogEntry;
use crate::repo::activity_log;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/activity", get(list).post(record))
}

#[derive(Deserialize)]
struct ListQuery {
    entity_type: Option<String>,
    entity_id: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<ActivityLogEntry>>> {
    Ok(Json(activity_log::list(
        &app.conn(),
        activity_log::ListFilter {
            entity_type: q.entity_type.as_deref(),
            entity_id: q.entity_id.as_deref(),
            limit: q.limit,
        },
    )?))
}

#[derive(Deserialize)]
struct RecordBody {
    entity_type: String,
    entity_id: String,
    action: String,
    field: Option<String>,
    old_value: Option<String>,
    new_value: Option<String>,
    actor: Option<String>,
}

async fn record(
    State(app): State<AppState>,
    Json(body): Json<RecordBody>,
) -> ApiResult<Json<ActivityLogEntry>> {
    Ok(Json(activity_log::record(
        &app.conn(),
        activity_log::RecordInput {
            entity_type: &body.entity_type,
            entity_id: &body.entity_id,
            action: &body.action,
            field: body.field.as_deref(),
            old_value: body.old_value.as_deref(),
            new_value: body.new_value.as_deref(),
            actor: body.actor.as_deref(),
        },
    )?))
}
