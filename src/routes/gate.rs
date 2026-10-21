// GET /gate/pre-tool-use?cwd=<abs>&session=<id>
//
// One read-only round trip for the Claude Code PreToolUse hook. The hook used
// to assemble this picture from ~7 CLI subprocesses per gated tool call
// (project resolve twice, dashboard, task list ×3, plan list, cycle list), and
// two of those task lists were not project-scoped, so a leftover in_progress
// task in another project satisfied this project's gate.
//
// Contract (consumed by clawket/adapters/shared/claude-hooks.cjs; field names
// are wire contract — add fields, never rename/remove):
//   {
//     "contract": 1,
//     "project": null | { "id", "key", "name", "enabled" },
//     "in_progress":   [Task],   // project-scoped
//     "blocked_ids":   [String], // project-scoped
//     "todo_assigned": [Task],   // project-scoped todo with an assignee
//     "active_plan_count":  Int,
//     "active_cycle_count": Int,
//     "session_run": null | { "id", "task_id" }
//   }
//
// Project resolution mirrors the hook's previous two-step read: an exact or
// prefix cwd match regardless of `enabled` (so a disabled project is reported
// as `enabled:false` and the hook can bypass enforcement), then the same
// sole-enabled-project fallback `/dashboard` applies. Unknown cwd with no
// fallback → `project: null` (the hook allows, as it did on an empty
// dashboard).

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::models::Project;
use crate::repo::{cycles, plans, projects, runs, tasks};
use crate::routes::error::ApiResult;
use crate::state::AppState;

pub const GATE_CONTRACT_VERSION: i64 = 1;

pub fn router() -> Router<AppState> {
    Router::new().route("/gate/pre-tool-use", get(pre_tool_use))
}

#[derive(Deserialize)]
struct GateQuery {
    cwd: Option<String>,
    session: Option<String>,
}

fn resolve_project(conn: &rusqlite::Connection, cwd: &str) -> anyhow::Result<Option<Project>> {
    if !cwd.is_empty() {
        if let Some(p) = projects::get_by_cwd(conn, cwd, false)? {
            return Ok(Some(p));
        }
    }
    let enabled: Vec<Project> = projects::list(conn)?
        .into_iter()
        .filter(|p| p.enabled != 0)
        .collect();
    if enabled.len() == 1 {
        return Ok(enabled.into_iter().next());
    }
    Ok(None)
}

async fn pre_tool_use(
    State(app): State<AppState>,
    Query(q): Query<GateQuery>,
) -> ApiResult<Json<Value>> {
    let conn = app.conn();
    let cwd = q.cwd.as_deref().unwrap_or("");
    let Some(project) = resolve_project(&conn, cwd)? else {
        return Ok(Json(json!({
            "contract": GATE_CONTRACT_VERSION,
            "project": null,
            "in_progress": [],
            "blocked_ids": [],
            "todo_assigned": [],
            "active_plan_count": 0,
            "active_cycle_count": 0,
            "session_run": null,
        })));
    };

    let scoped = |status: &'static str| {
        tasks::list(
            &conn,
            tasks::ListFilter {
                project_id: Some(&project.id),
                status: Some(status),
                ..Default::default()
            },
        )
    };
    let in_progress = scoped("in_progress")?;
    let blocked_ids: Vec<String> = scoped("blocked")?.into_iter().map(|t| t.id).collect();
    let todo_assigned: Vec<_> = scoped("todo")?
        .into_iter()
        .filter(|t| t.assignee.as_deref().is_some_and(|a| !a.is_empty()))
        .collect();
    let active_plan_count = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project.id),
            status: Some("active"),
        },
    )?
    .len();
    let active_cycle_count = cycles::list(
        &conn,
        cycles::ListFilter {
            project_id: Some(&project.id),
            unit_id: None,
            status: Some("active"),
        },
    )?
    .len();

    // Newest unfinished run for this session inside this project. `runs::list`
    // orders by started_at DESC, so the first open one is the newest.
    let session_run = match q.session.as_deref().filter(|s| !s.is_empty()) {
        Some(sid) => runs::list(
            &conn,
            runs::ListFilter {
                session_id: Some(sid),
                ..Default::default()
            },
        )?
        .into_iter()
        .filter(|r| r.ended_at.is_none())
        .find(|r| {
            runs::task_project_id(&conn, &r.task_id)
                .ok()
                .flatten()
                .as_deref()
                == Some(project.id.as_str())
        })
        .map(|r| json!({ "id": r.id, "task_id": r.task_id })),
        None => None,
    };

    Ok(Json(json!({
        "contract": GATE_CONTRACT_VERSION,
        "project": {
            "id": project.id,
            "key": project.key,
            "name": project.name,
            "enabled": project.enabled != 0,
        },
        "in_progress": in_progress,
        "blocked_ids": blocked_ids,
        "todo_assigned": todo_assigned,
        "active_plan_count": active_plan_count,
        "active_cycle_count": active_cycle_count,
        "session_run": session_run,
    })))
}

#[cfg(test)]
mod tests {
    use super::router;
    use crate::repo::runs;
    use crate::test_fixtures::{call, fixture, scope, task};
    use axum::http::StatusCode;
    use serde_json::Value;

    fn ids(v: &Value, field: &str) -> Vec<String> {
        v[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn returns_only_the_resolved_projects_tasks() {
        let fx = fixture();
        let a = scope(&fx, "Alpha", Some("/work/alpha"), "A plan");
        let b = scope(&fx, "Beta", Some("/work/beta"), "B plan");
        let (a_ip, a_blocked, a_todo, b_ip) = {
            let mut c = fx.state.conn();
            let a_ip = task(&mut c, &a, "a-ip", None, "in_progress");
            let a_blocked = task(&mut c, &a, "a-blocked", None, "blocked");
            let a_todo = task(&mut c, &a, "a-todo", Some("worker"), "todo");
            task(&mut c, &a, "a-todo-unassigned", None, "todo");
            // The leftover in another project that used to satisfy this gate.
            let b_ip = task(&mut c, &b, "b-ip", Some("worker"), "in_progress");
            task(&mut c, &b, "b-blocked", None, "blocked");
            (a_ip, a_blocked, a_todo, b_ip)
        };
        let app = router().with_state(fx.state.clone());
        let (st, v) = call(&app, "GET", "/gate/pre-tool-use?cwd=/work/alpha/src", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["contract"], 1);
        assert_eq!(v["project"]["id"], a.project_id.as_str());
        assert_eq!(v["project"]["enabled"], true);
        assert_eq!(ids(&v, "in_progress"), vec![a_ip.clone()]);
        assert!(!ids(&v, "in_progress").contains(&b_ip));
        assert_eq!(v["blocked_ids"], serde_json::json!([a_blocked]));
        assert_eq!(ids(&v, "todo_assigned"), vec![a_todo]);
        assert_eq!(v["active_plan_count"], 1);
        assert_eq!(v["active_cycle_count"], 1);
        assert!(v["session_run"].is_null());
        // Task JSON is the same shape GET /tasks returns.
        assert_eq!(v["in_progress"][0]["cycle_id"], a.cycle_id.as_str());
    }

    #[tokio::test]
    async fn unknown_cwd_with_no_sole_project_resolves_to_null() {
        let fx = fixture();
        scope(&fx, "Alpha", Some("/work/alpha"), "A plan");
        scope(&fx, "Beta", Some("/work/beta"), "B plan");
        let app = router().with_state(fx.state.clone());
        let (st, v) = call(&app, "GET", "/gate/pre-tool-use?cwd=/elsewhere", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["project"].is_null(), "{v}");
        assert_eq!(v["in_progress"], serde_json::json!([]));
        assert_eq!(v["active_plan_count"], 0);
    }

    #[tokio::test]
    async fn disabled_project_is_reported_not_hidden() {
        let fx = fixture();
        let a = scope(&fx, "Alpha", Some("/work/alpha"), "A plan");
        fx.state
            .conn()
            .execute(
                "UPDATE projects SET enabled = 0 WHERE id = ?1",
                rusqlite::params![a.project_id],
            )
            .unwrap();
        let app = router().with_state(fx.state.clone());
        let (_, v) = call(&app, "GET", "/gate/pre-tool-use?cwd=/work/alpha", None).await;
        assert_eq!(v["project"]["id"], a.project_id.as_str());
        assert_eq!(v["project"]["enabled"], false);
    }

    #[tokio::test]
    async fn session_run_is_the_open_run_of_this_session_in_this_project() {
        let fx = fixture();
        let a = scope(&fx, "Alpha", Some("/work/alpha"), "A plan");
        let b = scope(&fx, "Beta", Some("/work/beta"), "B plan");
        let run_a = {
            let mut c = fx.state.conn();
            let ta = task(&mut c, &a, "a-ip", None, "in_progress");
            let tb = task(&mut c, &b, "b-ip", None, "in_progress");
            let mk = |task_id: &str| {
                runs::create(
                    &c,
                    runs::CreateInput {
                        task_id,
                        session_id: Some("S1"),
                        agent: "main",
                        status: None,
                        envelope_id: None,
                        envelope_snapshot: None,
                    },
                )
                .unwrap()
                .unwrap()
                .id
            };
            let ra = mk(&ta);
            mk(&tb);
            ra
        };
        let app = router().with_state(fx.state.clone());
        let (_, v) = call(
            &app,
            "GET",
            "/gate/pre-tool-use?cwd=/work/alpha&session=S1",
            None,
        )
        .await;
        assert_eq!(v["session_run"]["id"], run_a.as_str(), "{v}");
        let (_, v) = call(
            &app,
            "GET",
            "/gate/pre-tool-use?cwd=/work/alpha&session=S2",
            None,
        )
        .await;
        assert!(v["session_run"].is_null());
    }
}
