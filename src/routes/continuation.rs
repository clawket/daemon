// Migration 027: GET /continuation?cwd=<cwd>
//
// Backs the Claude Code Stop hook's auto-advance feature. Given a working
// directory, resolve the project and its single active plan, and report the
// next actionable step so the hook can inject "keep going" instead of letting
// the agent stop mid-plan.
//
// Contract (consumed by adapters/shared/claude-hooks.cjs::runStop):
//   { "next": null }                              → allow stop
//   { "next": { "kind": "task"|"unit", "id", "title" }, "instruction": "..." }
//                                                 → block stop, inject instruction
//
// Auto-advance is opt-in per plan (plans.auto_advance). When the active plan
// has auto_advance=0 (or there is no active plan / no project), `next` is null
// so the legacy stop-at-end-of-turn behaviour is preserved.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::Project;
use crate::repo::plans::{self, NextStep};
use crate::repo::projects;
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/continuation", get(continuation))
}

#[derive(Deserialize)]
struct ContinuationQuery {
    cwd: Option<String>,
}

async fn continuation(
    State(app): State<AppState>,
    Query(q): Query<ContinuationQuery>,
) -> ApiResult<Json<Value>> {
    let cwd = q.cwd.as_deref().unwrap_or("");
    let conn = app.conn();

    // Resolve project: prefer exact cwd match, fall back to the sole project
    // when there is exactly one (mirrors handoff.rs resolution).
    let mut project: Option<Project> = if !cwd.is_empty() {
        projects::get_by_cwd(&conn, cwd, false)?
    } else {
        None
    };
    if project.is_none() {
        let all = projects::list(&conn)?;
        if all.len() == 1 {
            project = all.into_iter().next();
        }
    }
    let Some(project) = project else {
        return Ok(Json(json!({ "next": null })));
    };

    // Single active plan per project (FIX-DAEMON-017). Only `active` plans
    // auto-advance — draft/approved are not yet running.
    let active_plan = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project.id),
            status: Some("active"),
        },
    )?
    .into_iter()
    .next();

    let Some(plan) = active_plan else {
        return Ok(Json(json!({ "next": null })));
    };

    // Opt-in gate: only plans with auto_advance=1 drive the Stop hook.
    if !plan.auto_advance {
        return Ok(Json(json!({ "next": null })));
    }

    match plans::next_actionable(&conn, &plan.id)? {
        Some(NextStep::Task { id, title }) => Ok(Json(json!({
            "next": { "kind": "task", "id": id, "title": title },
            "instruction": format!(
                "다음 태스크를 진행하라: {title} ({id}). 시작 후 완료까지 수행."
            ),
        }))),
        Some(NextStep::Unit { id, title }) => Ok(Json(json!({
            "next": { "kind": "unit", "id": id, "title": title },
            "instruction": format!(
                "다음 페이즈(unit)로 진행: {title} ({id}). 이 페이즈의 태스크를 생성하고 수행하라."
            ),
        }))),
        None => Ok(Json(json!({ "next": null }))),
    }
}
