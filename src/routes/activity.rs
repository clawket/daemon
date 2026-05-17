use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::jobs::activity_log_rollup::{current_size_bytes, RetentionPolicy};
use crate::models::ActivityLogEntry;
use crate::repo::activity_log;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/activity", get(list).post(record))
        .route("/activity/stats", get(stats))
}

#[derive(Serialize)]
struct ActivityStats {
    used_bytes: i64,
    max_bytes: i64,
    hot_rows: i64,
    archive_batches: i64,
    archive_oldest_period_start: Option<i64>,
    hot_days: i64,
    total_days: i64,
    max_mb: i64,
}

async fn stats(State(app): State<AppState>) -> ApiResult<Json<ActivityStats>> {
    let policy = RetentionPolicy::from_env();
    let conn = app.conn();
    let used_bytes = current_size_bytes(&conn)?;
    let hot_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM activity_log", [], |r| r.get(0))
        .unwrap_or(0);
    let archive_batches: i64 = conn
        .query_row("SELECT COUNT(*) FROM activity_log_archive", [], |r| r.get(0))
        .unwrap_or(0);
    let archive_oldest_period_start: Option<i64> = conn
        .query_row(
            "SELECT MIN(period_start) FROM activity_log_archive",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(Json(ActivityStats {
        used_bytes,
        max_bytes: policy.max_mb.saturating_mul(1024 * 1024),
        hot_rows,
        archive_batches,
        archive_oldest_period_start,
        hot_days: policy.hot_days,
        total_days: policy.total_days,
        max_mb: policy.max_mb,
    }))
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
