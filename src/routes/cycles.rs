use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::{Cycle, CycleCounts, Task};
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
        .route("/cycles/{id}/counts", get(counts))
        // US-CLAWKET-PDD-230: cycle-level scenario diff against another cycle.
        .route("/cycles/{id}/diff", get(cycle_diff))
}

#[derive(Deserialize)]
struct ListQuery {
    project_id: Option<String>,
    unit_id: Option<String>,
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
            unit_id: q.unit_id.as_deref(),
            status: q.status.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    project_id: String,
    /// FIX-DAEMON-002: unit_id required (PDD A4: Cycle ⊂ Unit)
    unit_id: String,
    title: String,
    goal: Option<String>,
    idx: Option<i64>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Cycle>> {
    let goal = norm_opt(body.goal);
    if body.unit_id.trim().is_empty() {
        return Err(ApiError::bad_request_coded(
            "MISSING_UNIT_ID",
            "MISSING_UNIT_ID: unit_id is required for cycle creation (PDD A4: Cycle ⊂ Unit). Use: clawket cycle create --unit <UNIT-ID>",
        ));
    }
    // Validate unit exists (UNIT_NOT_FOUND)
    {
        use crate::repo::units;
        let unit = units::get(&app.conn(), &body.unit_id)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if unit.is_none() {
            return Err(ApiError::not_found_coded(
                "UNIT_NOT_FOUND",
                format!("UNIT_NOT_FOUND: unit '{}' does not exist", body.unit_id),
            ));
        }
    }
    json_or_404(cycles::create(
        &app.conn(),
        cycles::CreateInput {
            project_id: &body.project_id,
            unit_id: &body.unit_id,
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

/// FIX-DAEMON-101: /cycles/:id/counts — SQL aggregate task counts for one cycle
async fn counts(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<CycleCounts>> {
    let conn = app.conn();
    let cycle = cycles::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("cycle not found"))?;

    let row: (i64, i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT
            SUM(CASE WHEN status = 'todo'        THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'in_progress' THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'done'        THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'blocked'     THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'cancelled'   THEN 1 ELSE 0 END),
            COUNT(id)
         FROM tasks WHERE cycle_id = ?1",
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

    Ok(Json(CycleCounts {
        cycle_id: cycle.id,
        cycle_title: cycle.title,
        todo: row.0,
        in_progress: row.1,
        done: row.2,
        blocked: row.3,
        cancelled: row.4,
        total: row.5,
    }))
}

// ---------------------------------------------------------------------------
// US-CLAWKET-PDD-230: cycle-level scenario diff (`?against=<other_cycle_id>`)
// ---------------------------------------------------------------------------
#[derive(Deserialize)]
struct CycleDiffQuery {
    against: Option<String>,
}

/// Sibling of `/plans/{id}/rounds/{n}/diff` — same shape, but cycle-scoped.
/// `against` defaults to "the cycle that comes before this one in the same
/// unit ordered by created_at" so callers can call it without pre-resolving
/// the prior cycle. PDD-230.
async fn cycle_diff(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<CycleDiffQuery>,
) -> ApiResult<Json<Value>> {
    let conn = app.conn();
    let current = cycles::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("cycle not found"))?;

    let against_id: Option<String> = match q.against {
        Some(s) if !s.is_empty() => Some(s),
        _ => {
            // Fall back to the prior cycle in the same unit (created_at < current.created_at).
            current.unit_id.as_ref().and_then(|uid| {
                conn.query_row(
                    "SELECT id FROM cycles
                     WHERE unit_id = ?1 AND created_at < ?2 AND id != ?3
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1",
                    rusqlite::params![uid, current.created_at, id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            })
        }
    };

    let prior_tasks = match against_id.as_ref() {
        Some(c) => crate::routes::plans::round_tasks_for_cycle(&conn, c)?,
        None => Vec::new(),
    };
    let cur_tasks = crate::routes::plans::round_tasks_for_cycle(&conn, &id)?;

    let diff = crate::routes::plans::compute_round_diff(&prior_tasks, &cur_tasks);

    Ok(Json(serde_json::json!({
        "cycle_id": id,
        "against_cycle_id": against_id,
        "flipped_pass": diff.flipped_pass,
        "flipped_defect": diff.flipped_defect,
        "still_pass": diff.still_pass,
        "still_defect": diff.still_defect,
        "new": diff.new_,
        "removed": diff.removed,
    })))
}
