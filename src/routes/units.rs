// FIX-DAEMON-004: Units are pure grouping entities — removed approve endpoint,
// status/approval_required from CreateBody.
// FIX-DAEMON-101: /units/:id/counts with SQL aggregate
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::{SingleUnitCounts, Unit};
use crate::repo::units;
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/units", get(list).post(create))
        .route("/units/{id}", get(get_one).patch(update).delete(delete_one))
        .route("/units/{id}/counts", get(counts))
    // NOTE: /units/{id}/approve endpoint removed in v3.0 (FIX-DAEMON-004).
    // Units are pure grouping entities with no approval lifecycle.
}

#[derive(Deserialize)]
struct ListQuery {
    plan_id: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Unit>>> {
    Ok(Json(units::list(
        &app.conn(),
        units::ListFilter {
            plan_id: q.plan_id.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    plan_id: String,
    title: String,
    goal: Option<String>,
    idx: Option<i64>,
    execution_mode: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Unit>> {
    json_or_404(units::create(
        &app.conn(),
        units::CreateInput {
            plan_id: &body.plan_id,
            title: &body.title,
            goal: body.goal.as_deref(),
            idx: body.idx,
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

/// FIX-DAEMON-101: /units/:id/counts — SQL aggregate task counts for one unit
async fn counts(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SingleUnitCounts>> {
    let conn = app.conn();
    let unit = units::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("unit not found"))?;

    let row: (i64, i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT
            SUM(CASE WHEN status = 'todo'        THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'in_progress' THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'done'        THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'blocked'     THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'cancelled'   THEN 1 ELSE 0 END),
            COUNT(id)
         FROM tasks WHERE unit_id = ?1",
            rusqlite::params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(SingleUnitCounts {
        unit_id: unit.id,
        unit_title: unit.title,
        todo: row.0,
        in_progress: row.1,
        done: row.2,
        blocked: row.3,
        cancelled: row.4,
        total: row.5,
    }))
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
    if let Some(s) = obj.get("execution_mode").and_then(Value::as_str) {
        f.execution_mode = Some(s.into());
    }
    let result = units::update(&app.conn(), &id, f)?;
    if result.is_some() {
        app.emit("unit:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}
