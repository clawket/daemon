// FIX-DAEMON-003: plan completion residue gate
// FIX-DAEMON-016: /plans/:id/counts endpoint
// FIX-DAEMON-017: single-active-plan invariant
// FIX-DAEMON-101: /plans/:id/counts rewritten with SQL aggregate (no N+1)
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::{Plan, PlanCounts, UnitCounts};
use crate::repo::{plans, units};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::{
    norm_opt, resolve_project_ref, resolve_project_ref_opt, value_to_opt_string,
};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans", get(list).post(create))
        .route("/plans/{id}", get(get_one).patch(update).delete(delete_one))
        .route("/plans/{id}/approve", post(approve))
        .route("/plans/{id}/counts", get(counts))
        // US-CLAWKET-PDD-113: QA round comparator (task delta vs N-1).
        .route("/plans/{id}/rounds/{n}/diff", get(round_diff))
        // US-CLAWKET-PDD-231: regression-intent set (pass→defect across boundary).
        .route(
            "/plans/{id}/rounds/{n}/regression-intent",
            get(round_regression_intent),
        )
}

/// US-CLAWKET-PDD-111: helper builder for the
/// `{ error: "single_active_plan", existing_plan_id }` 409 response.
fn single_active_plan_conflict(existing_id: &str) -> ApiError {
    ApiError::conflict_flat(
        "single_active_plan",
        serde_json::json!({ "existing_plan_id": existing_id }),
    )
}

#[derive(Deserialize)]
struct ListQuery {
    project_id: Option<String>,
    status: Option<String>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Plan>>> {
    let conn = app.conn();
    let project_id = resolve_project_ref_opt(&conn, q.project_id.as_deref())?;
    Ok(Json(plans::list(
        &conn,
        plans::ListFilter {
            project_id: project_id.as_deref(),
            status: q.status.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    project_id: String,
    title: String,
    description: Option<String>,
    source: Option<String>,
    source_path: Option<String>,
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<Plan>> {
    let description = norm_opt(body.description);
    let source = norm_opt(body.source);
    let source_path = norm_opt(body.source_path);
    let conn = app.conn();
    let project_id = resolve_project_ref(&conn, &body.project_id)?;
    json_or_404(plans::create(
        &conn,
        plans::CreateInput {
            project_id: &project_id,
            title: &body.title,
            description: description.as_deref(),
            source: source.as_deref(),
            source_path: source_path.as_deref(),
        },
    )?)
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Plan>> {
    json_or_404(plans::get(&app.conn(), &id)?)
}

async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    plans::delete(&app.conn(), &id)?;
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

async fn approve(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Plan>> {
    // US-CLAWKET-PDD-111 / FIX-DAEMON-017: single-active-plan invariant. The
    // scenario expects HTTP 409 with body shape
    //   { error: "single_active_plan", existing_plan_id: "<id>" }
    // so callers can disambiguate without parsing the human-readable text.
    let conn = app.conn();
    let plan = plans::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("plan not found"))?;

    let existing_active_id: Option<String> = conn
        .query_row(
            "SELECT id FROM plans WHERE project_id = ?1 AND status = 'active' AND id != ?2 LIMIT 1",
            rusqlite::params![plan.project_id, id],
            |r| r.get::<_, String>(0),
        )
        .ok();

    if let Some(existing_id) = existing_active_id {
        return Err(single_active_plan_conflict(&existing_id));
    }
    drop(conn);

    let result = plans::approve(&app.conn(), &id)?;
    if result.is_some() {
        app.emit("plan:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Plan>> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = plans::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("description") {
        f.description = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        // FIX-DAEMON-017: block draft→active via PATCH; require /approve endpoint
        if s == "active" {
            if let Some(existing) = plans::get(&app.conn(), &id)? {
                if existing.status == "draft" {
                    return Err(ApiError::bad_request(
                        "Use POST /plans/:id/approve to activate a draft plan",
                    ));
                }
            }
            // US-CLAWKET-PDD-111: SINGLE_ACTIVE_PLAN invariant — return 409
            // with `{ error: "single_active_plan", existing_plan_id: "<id>" }`
            // body so the client can navigate to the conflicting plan directly.
            // FIX-DAEMON-103: bind `existing` first so the temporary MutexGuard
            // from `&app.conn()` drops at the end of the let-statement; otherwise
            // the next `app.conn()` (line ~157) re-locks the same std::sync::Mutex
            // from the same thread → POSIX-undefined / macOS deadlock.
            let existing_active = plans::get(&app.conn(), &id)?;
            if let Some(existing) = existing_active {
                let existing_active_id: Option<String> = app
                    .conn()
                    .query_row(
                        "SELECT id FROM plans WHERE project_id = ?1 AND status = 'active' AND id != ?2 LIMIT 1",
                        rusqlite::params![existing.project_id, id],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                if let Some(existing_id) = existing_active_id {
                    return Err(single_active_plan_conflict(&existing_id));
                }
            }
        }

        // FIX-DAEMON-003 / PDD-113 / PDD-231: completion residue gate + cascade close
        if s == "completed" {
            // FIX-DAEMON-103: same self-deadlock pattern as above. Bind first.
            let existing_completed = plans::get(&app.conn(), &id)?;
            if let Some(existing) = existing_completed {
                if existing.status == "active" {
                    let conn = app.conn();
                    // PDD-113: cascade close — set any active cycle for this plan to completed.
                    // We only auto-close `active` cycles; `planning` cycles are explicitly NOT
                    // auto-closed because they may have unfinished work the user wants kept.
                    let now = crate::id::now_ms();
                    let _ = conn.execute(
                        "UPDATE cycles SET status = 'completed', ended_at = ?2
                         WHERE id IN (SELECT c.id FROM cycles c
                                      JOIN units u ON c.unit_id = u.id
                                      WHERE u.plan_id = ?1 AND c.status = 'active')",
                        rusqlite::params![id, now],
                    );
                    // PDD-231: defensive — after cascade, reject if any active cycle remains.
                    let active_cycles: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM cycles c
                         JOIN units u ON c.unit_id = u.id
                         WHERE u.plan_id = ?1 AND c.status = 'active'",
                            rusqlite::params![id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if active_cycles > 0 {
                        return Err(ApiError::conflict_coded(
                            "PLAN_HAS_ACTIVE_CYCLES",
                            format!(
                                "PLAN_HAS_ACTIVE_CYCLES: plan still has {} active cycle(s) after cascade close.",
                                active_cycles
                            ),
                        ));
                    }
                    // Existing residue gate: planning cycles + non-terminal tasks still block.
                    let pending_cycles: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM cycles c
                         JOIN units u ON c.unit_id = u.id
                         WHERE u.plan_id = ?1 AND c.status != 'completed'",
                            rusqlite::params![id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    let pending_tasks: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM tasks t
                         JOIN units u ON t.unit_id = u.id
                         WHERE u.plan_id = ?1 AND t.status NOT IN ('done', 'cancelled')",
                            rusqlite::params![id],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if pending_cycles > 0 || pending_tasks > 0 {
                        return Err(ApiError::conflict(format!(
                            "Cannot complete plan: {} planning cycle(s) and {} non-terminal task(s) remain",
                            pending_cycles, pending_tasks
                        )));
                    }
                }
            }
        }

        f.status = Some(s.into());
    }
    if let Some(v) = obj.get("approved_at") {
        f.approved_at = Some(v.as_i64());
    }
    let result = plans::update(&app.conn(), &id, f)?;
    if result.is_some() {
        app.emit("plan:updated", serde_json::json!({ "id": id }));
    }
    json_or_404(result)
}

/// FIX-DAEMON-016 / FIX-DAEMON-101: /plans/:id/counts — single SQL aggregate (no N+1)
async fn counts(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<PlanCounts>> {
    let conn = app.conn();
    let plan = plans::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("plan not found"))?;

    // Single aggregate query: group tasks by unit
    let mut stmt = conn.prepare(
        "SELECT u.id, u.title,
                SUM(CASE WHEN t.status = 'todo'        THEN 1 ELSE 0 END),
                SUM(CASE WHEN t.status = 'in_progress' THEN 1 ELSE 0 END),
                SUM(CASE WHEN t.status = 'done'        THEN 1 ELSE 0 END),
                SUM(CASE WHEN t.status = 'blocked'     THEN 1 ELSE 0 END),
                SUM(CASE WHEN t.status = 'cancelled'   THEN 1 ELSE 0 END),
                COUNT(t.id)
         FROM units u
         LEFT JOIN tasks t ON t.unit_id = u.id
         WHERE u.plan_id = ?1
         GROUP BY u.id, u.title
         ORDER BY u.idx",
    )?;

    let rows = stmt.query_map(rusqlite::params![id], |r| {
        Ok(UnitCounts {
            unit_id: r.get(0)?,
            unit_title: r.get(1)?,
            todo: r.get(2)?,
            in_progress: r.get(3)?,
            done: r.get(4)?,
            blocked: r.get(5)?,
            cancelled: r.get(6)?,
            total: r.get(7)?,
        })
    })?;

    let mut unit_counts_vec = Vec::new();
    let mut total_todo = 0i64;
    let mut total_in_progress = 0i64;
    let mut total_done = 0i64;
    let mut total_blocked = 0i64;
    let mut total_cancelled = 0i64;

    for r in rows {
        let uc = r?;
        total_todo += uc.todo;
        total_in_progress += uc.in_progress;
        total_done += uc.done;
        total_blocked += uc.blocked;
        total_cancelled += uc.cancelled;
        unit_counts_vec.push(uc);
    }

    // If plan has no units, also check if plan has units from the units list
    // to return proper empty shape
    if unit_counts_vec.is_empty() {
        let plan_units = units::list(&conn, units::ListFilter { plan_id: Some(&id) })?;
        for u in plan_units {
            unit_counts_vec.push(UnitCounts {
                unit_id: u.id,
                unit_title: u.title,
                todo: 0,
                in_progress: 0,
                done: 0,
                blocked: 0,
                cancelled: 0,
                total: 0,
            });
        }
    }

    let total_tasks = total_todo + total_in_progress + total_done + total_blocked + total_cancelled;

    Ok(Json(PlanCounts {
        plan_id: plan.id,
        units: unit_counts_vec,
        total_todo,
        total_in_progress,
        total_done,
        total_blocked,
        total_cancelled,
        total_tasks,
    }))
}

// ---------------------------------------------------------------------------
// US-CLAWKET-PDD-113 / PDD-231: round comparators
// ---------------------------------------------------------------------------

/// Internal slice of a task used for round delta classification.
#[derive(Debug, Clone)]
pub(crate) struct RoundTask {
    pub task_id: String,
    pub scenario_id: String,
    pub qa_status: String,    // "pass" | "defect" | "scenario_error" | ""
    pub body: Option<String>, // doubles as `evidence` carrier when emitting JSON
}

/// Resolve the `round` index → cycle id for a plan. Cycles in the plan are
/// sorted by `created_at` ascending; round 1 is the earliest. Returns the
/// matching cycle id or `None` when the plan has fewer rounds than `n`.
/// US-CLAWKET-PDD-113.
fn cycle_for_round(
    conn: &rusqlite::Connection,
    plan_id: &str,
    round: i64,
) -> Result<Option<String>, ApiError> {
    if round < 1 {
        return Ok(None);
    }
    let offset = round - 1;
    let row: Option<String> = conn
        .query_row(
            "SELECT c.id FROM cycles c
             JOIN units u ON c.unit_id = u.id
             WHERE u.plan_id = ?1
             ORDER BY c.created_at ASC, c.id ASC
             LIMIT 1 OFFSET ?2",
            rusqlite::params![plan_id, offset],
            |r| r.get::<_, String>(0),
        )
        .ok();
    Ok(row)
}

pub(crate) fn round_tasks_for_cycle(
    conn: &rusqlite::Connection,
    cycle_id: &str,
) -> Result<Vec<RoundTask>, ApiError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, COALESCE(scenario_id, ''), COALESCE(qa_status, ''), body
             FROM tasks
             WHERE cycle_id = ?1 AND COALESCE(scenario_id, '') != ''",
        )
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![cycle_id], |r| {
            Ok(RoundTask {
                task_id: r.get::<_, String>(0)?,
                scenario_id: r.get::<_, String>(1)?,
                qa_status: r.get::<_, String>(2)?,
                body: r.get::<_, Option<String>>(3)?,
            })
        })
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| ApiError::internal(e.to_string()))?);
    }
    Ok(out)
}

#[derive(Debug, Default)]
pub(crate) struct RoundDiff {
    pub flipped_pass: Vec<Value>,
    pub flipped_defect: Vec<Value>,
    pub still_pass: Vec<Value>,
    pub still_defect: Vec<Value>,
    pub new_: Vec<Value>,
    pub removed: Vec<Value>,
}

/// Compute the diff between two task slices grouped by `scenario_id`. Each
/// slice represents one round; classification is symmetric. US-CLAWKET-PDD-113.
pub(crate) fn compute_round_diff(prior: &[RoundTask], current: &[RoundTask]) -> RoundDiff {
    use std::collections::HashMap;
    let mut prior_map: HashMap<&str, &RoundTask> = HashMap::new();
    for t in prior {
        prior_map.insert(t.scenario_id.as_str(), t);
    }
    let mut current_map: HashMap<&str, &RoundTask> = HashMap::new();
    for t in current {
        current_map.insert(t.scenario_id.as_str(), t);
    }

    let mut out = RoundDiff::default();

    for (sid, cur) in current_map.iter() {
        match prior_map.get(sid) {
            Some(prv) => {
                let prev_status = prv.qa_status.as_str();
                let cur_status = cur.qa_status.as_str();
                let entry = serde_json::json!({
                    "scenario_id": sid,
                    "prior_task_id": prv.task_id,
                    "current_task_id": cur.task_id,
                    "prior_status": prev_status,
                    "current_status": cur_status,
                });
                match (prev_status, cur_status) {
                    ("defect", "pass") => out.flipped_pass.push(entry),
                    ("pass", "defect") => out.flipped_defect.push(entry),
                    ("pass", "pass") => out.still_pass.push(entry),
                    ("defect", "defect") => out.still_defect.push(entry),
                    _ => {
                        // Other transitions (e.g. scenario_error, unknown) are
                        // surfaced under `still_*` based on current status to
                        // avoid silently dropping them.
                        if cur_status == "pass" {
                            out.still_pass.push(entry);
                        } else if cur_status == "defect" {
                            out.still_defect.push(entry);
                        }
                    }
                }
            }
            None => {
                out.new_.push(serde_json::json!({
                    "scenario_id": sid,
                    "current_task_id": cur.task_id,
                    "current_status": cur.qa_status,
                }));
            }
        }
    }

    for (sid, prv) in prior_map.iter() {
        if !current_map.contains_key(sid) {
            out.removed.push(serde_json::json!({
                "scenario_id": sid,
                "prior_task_id": prv.task_id,
                "prior_status": prv.qa_status,
            }));
        }
    }

    out
}

/// US-CLAWKET-PDD-113: GET /plans/{id}/rounds/{n}/diff — task-level delta
/// between round N and round N-1 of the same plan, grouped by scenario_id.
async fn round_diff(
    State(app): State<AppState>,
    Path((id, n)): Path<(String, i64)>,
) -> ApiResult<Json<Value>> {
    if n < 2 {
        return Err(ApiError::bad_request(
            "round must be >= 2 to compare against the prior round",
        ));
    }
    let conn = app.conn();
    let _plan = plans::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("plan not found"))?;

    let prior_cycle = cycle_for_round(&conn, &id, n - 1)?;
    let cur_cycle = cycle_for_round(&conn, &id, n)?;

    let prior_tasks = match prior_cycle {
        Some(ref c) => round_tasks_for_cycle(&conn, c)?,
        None => Vec::new(),
    };
    let cur_tasks = match cur_cycle {
        Some(ref c) => round_tasks_for_cycle(&conn, c)?,
        None => Vec::new(),
    };

    let diff = compute_round_diff(&prior_tasks, &cur_tasks);

    Ok(Json(serde_json::json!({
        "plan_id": id,
        "from_round": n - 1,
        "to_round": n,
        "flipped_pass": diff.flipped_pass,
        "flipped_defect": diff.flipped_defect,
        "still_pass": diff.still_pass,
        "still_defect": diff.still_defect,
        "new": diff.new_,
        "removed": diff.removed,
    })))
}

/// US-CLAWKET-PDD-231: GET /plans/{id}/rounds/{n}/regression-intent — the
/// pass→defect set across the boundary between round N-1 and round N. Used
/// by QA flow to surface regressions introduced by intervening fixes.
async fn round_regression_intent(
    State(app): State<AppState>,
    Path((id, n)): Path<(String, i64)>,
) -> ApiResult<Json<Value>> {
    if n < 2 {
        return Err(ApiError::bad_request(
            "round must be >= 2 to compute regression-intent",
        ));
    }
    let conn = app.conn();
    let _plan = plans::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("plan not found"))?;

    let prior_cycle = cycle_for_round(&conn, &id, n - 1)?;
    let cur_cycle = cycle_for_round(&conn, &id, n)?;

    let prior_tasks = match prior_cycle {
        Some(ref c) => round_tasks_for_cycle(&conn, c)?,
        None => Vec::new(),
    };
    let cur_tasks = match cur_cycle {
        Some(ref c) => round_tasks_for_cycle(&conn, c)?,
        None => Vec::new(),
    };

    use std::collections::HashMap;
    let mut prior_map: HashMap<&str, &RoundTask> = HashMap::new();
    for t in prior_tasks.iter() {
        prior_map.insert(t.scenario_id.as_str(), t);
    }

    let mut regressed: Vec<Value> = Vec::new();
    for cur in cur_tasks.iter() {
        if cur.qa_status != "defect" {
            continue;
        }
        let Some(prv) = prior_map.get(cur.scenario_id.as_str()) else {
            continue;
        };
        if prv.qa_status != "pass" {
            continue;
        }
        regressed.push(serde_json::json!({
            "scenario_id": cur.scenario_id,
            "prior_round_status": "pass",
            "current_round_status": "defect",
            "current_evidence": cur.body.clone().unwrap_or_default(),
            "prior_task_id": prv.task_id,
            "current_task_id": cur.task_id,
        }));
    }

    Ok(Json(serde_json::json!({
        "plan_id": id,
        "from_round": n - 1,
        "to_round": n,
        "regressed": regressed,
    })))
}
