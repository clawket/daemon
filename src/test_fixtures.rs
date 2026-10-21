//! Shared in-process fixtures for route-level tests: a migrated temp DB, an
//! `AppState`, and terse builders for project → plan → unit → cycle → task.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::db::Db;
use crate::paths::Paths;
use crate::repo::{cycles, plans, projects, tasks, units};
use crate::state::AppState;

pub struct Fx {
    pub _dir: tempfile::TempDir,
    pub state: AppState,
}

pub fn paths(root: &std::path::Path) -> Paths {
    let cache = root.join("cache");
    Paths {
        data: root.join("data"),
        cache: cache.clone(),
        config: root.join("config"),
        state: root.join("state"),
        db: root.join("db.sqlite"),
        port_file: cache.join("port"),
        pid_file: cache.join("pid"),
        socket: cache.join("sock"),
        token_file: cache.join("token"),
        web_dir: None,
    }
}

pub fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("fx.db")).unwrap();
    let state = AppState::new(db, paths(dir.path()), String::new());
    Fx { _dir: dir, state }
}

/// A project with one active plan, one unit and one active cycle.
pub struct Scope {
    pub project_id: String,
    pub plan_id: String,
    pub unit_id: String,
    pub cycle_id: String,
}

pub fn scope(fx: &Fx, name: &str, cwd: Option<&str>, plan_title: &str) -> Scope {
    let mut conn = fx.state.conn();
    let project = projects::create(
        &mut conn,
        projects::CreateInput {
            name,
            description: None,
            cwd,
            key: None,
        },
    )
    .unwrap()
    .unwrap();
    plan_in(&mut conn, &project.id, plan_title)
}

pub fn plan_in(conn: &mut rusqlite::Connection, project_id: &str, plan_title: &str) -> Scope {
    let plan = plans::create(
        conn,
        plans::CreateInput {
            project_id,
            title: plan_title,
            description: None,
            source: None,
            source_path: None,
            auto_advance: false,
        },
    )
    .unwrap()
    .unwrap();
    plans::approve(conn, &plan.id).unwrap();
    let unit = units::create(
        conn,
        units::CreateInput {
            plan_id: &plan.id,
            title: "U1",
            goal: None,
            idx: None,
            execution_mode: None,
        },
    )
    .unwrap()
    .unwrap();
    let cycle = cycles::create(
        conn,
        cycles::CreateInput {
            project_id,
            unit_id: &unit.id,
            title: "C1",
            goal: None,
            idx: None,
        },
    )
    .unwrap()
    .unwrap();
    cycles::activate(conn, &cycle.id).unwrap();
    Scope {
        project_id: project_id.to_string(),
        plan_id: plan.id,
        unit_id: unit.id,
        cycle_id: cycle.id,
    }
}

pub fn task(
    conn: &mut rusqlite::Connection,
    sc: &Scope,
    title: &str,
    assignee: Option<&str>,
    status: &str,
) -> String {
    let t = tasks::create(
        conn,
        tasks::CreateInput {
            unit_id: &sc.unit_id,
            idx: None,
            title,
            body: None,
            priority: None,
            complexity: None,
            estimated_edits: None,
            cycle_id: Some(&sc.cycle_id),
            reporter: None,
            type_: None,
            assignee,
            depends_on: vec![],
            parent_task_id: None,
            atomic_size_hint: None,
            decomposition_policy: None,
            tier: None,
            qa_status: None,
            scenario_id: None,
            defect_task: None,
            scenario_amendment: None,
            evidence: None,
            batch_id: None,
        },
    )
    .unwrap()
    .unwrap();
    let set = |conn: &mut rusqlite::Connection, s: &str, reason: Option<&str>| {
        tasks::update(
            conn,
            &t.id,
            tasks::UpdateFields {
                status: Some(s.to_string()),
                blocked_reason: reason.map(|r| Some(r.to_string())),
                ..Default::default()
            },
        )
        .unwrap();
    };
    match status {
        "todo" => {}
        "in_progress" => set(conn, "in_progress", None),
        "blocked" => set(conn, "blocked", Some("waiting on fixture")),
        other => panic!("fixture status not supported: {other}"),
    }
    t.id
}

pub async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
