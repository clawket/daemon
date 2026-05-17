use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::models::Run;
use crate::repo::{runs, task_envelopes, tasks};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list).post(create))
        .route("/runs/{id}", get(get_one))
        .route("/runs/{id}/start", post(start))
        .route("/runs/{id}/finish", post(finish))
        // API-RUN-004: replay endpoint — returns entity snapshot at given timestamp
        .route("/entities/{id}/replay", get(replay_at))
}

#[derive(Deserialize)]
struct ReplayQ {
    at: Option<String>,
}

/// US-CLAWKET-API-RUN-004: replay an entity's state at a given ISO 8601 timestamp.
/// Folds audit_log rows for the entity (recorded_at <= `at`) into a flat field
/// snapshot by applying each row's `field`/`new_value` pair in chronological
/// order. Returns `{ snapshot, replayed_through, audit_count }`. When no
/// `at` is supplied, the cutoff is "now" (i.e. fold the entire history).
async fn replay_at(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ReplayQ>,
) -> ApiResult<Json<Value>> {
    let conn = app.conn();
    // SQL ISO 8601 strings sort lexicographically. Compare directly when caller
    // supplies a cutoff; otherwise read all rows.
    let cutoff = q.at.as_deref().unwrap_or("");
    let mut stmt = conn
        .prepare(
            "SELECT id, op_type, field, old_value, new_value, actor, at
             FROM audit_log
             WHERE entity_id = ?1 AND (?2 = '' OR at <= ?2)
             ORDER BY at ASC, rowid ASC",
        )
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let rows: Vec<(String, String, Option<String>, Option<String>, Option<String>, String, String)> =
        stmt
            .query_map(rusqlite::params![id, cutoff], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| ApiError::internal(e.to_string()))?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| ApiError::internal(e.to_string()))?;

    let audit_count = rows.len() as i64;
    let mut snapshot = serde_json::Map::new();
    let mut last_at: Option<String> = None;

    for (_id, op_type, field, _old_value, new_value, _actor, at) in rows.iter() {
        last_at = Some(at.clone());
        match op_type.as_str() {
            "deleted" => {
                // Entity was deleted at this point in history — clear snapshot.
                snapshot.clear();
            }
            _ => {
                if let Some(f) = field {
                    let v = match new_value {
                        Some(s) => match serde_json::from_str::<Value>(s) {
                            Ok(parsed) => parsed,
                            Err(_) => Value::String(s.clone()),
                        },
                        None => Value::Null,
                    };
                    snapshot.insert(f.clone(), v);
                }
            }
        }
    }

    let snapshot_value: Value = if audit_count == 0 {
        Value::Null
    } else {
        Value::Object(snapshot)
    };
    let replayed_through = last_at.unwrap_or_else(|| q.at.clone().unwrap_or_default());

    Ok(Json(serde_json::json!({
        "entity_id": id,
        "snapshot": snapshot_value,
        "replayed_through": replayed_through,
        "audit_count": audit_count,
    })))
}

#[derive(Deserialize)]
struct ListQuery {
    task_id: Option<String>,
    session_id: Option<String>,
    project_id: Option<String>,
}

async fn list(State(app): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Run>>> {
    Ok(Json(runs::list(
        &app.conn(),
        runs::ListFilter {
            task_id: q.task_id.as_deref(),
            session_id: q.session_id.as_deref(),
            project_id: q.project_id.as_deref(),
        },
    )?))
}

#[derive(Deserialize)]
struct CreateBody {
    task_id: String,
    session_id: Option<String>,
    agent: Option<String>,
    /// "pending" or "started" (default). "pending" lets execute_task hand back
    /// a prompt + snapshot without claiming the task as actively running yet.
    status: Option<String>,
    /// Client-supplied resolved envelope. If omitted, the daemon resolves and
    /// freezes the active envelope chain at this moment. If the task has no
    /// active envelope, the snapshot is null and envelope_id is unset.
    envelope_snapshot: Option<Value>,
}

async fn create(State(app): State<AppState>, Json(body): Json<CreateBody>) -> ApiResult<Json<Run>> {
    let conn = app.conn();

    let task = tasks::get(&conn, &body.task_id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", body.task_id)))?;

    let status = body.status.as_deref().unwrap_or("started");
    if !matches!(status, "pending" | "started") {
        return Err(ApiError::bad_request(format!(
            "status must be 'pending' or 'started', got '{}'",
            status
        )));
    }

    // FIX-DAEMON-111: 409 RUN_ALREADY_OPEN — prevent creating a new run when an open one exists
    let open_run_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND ended_at IS NULL",
            rusqlite::params![task.id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if open_run_exists {
        return Err(ApiError::conflict_coded(
            "RUN_ALREADY_OPEN",
            format!(
                "RUN_ALREADY_OPEN: task '{}' already has an open run. Finish it first.",
                task.id
            ),
        ));
    }

    let active_env = task_envelopes::active_for_task(&conn, &task.id)?;
    let envelope_id = active_env.as_ref().map(|e| e.id.clone());

    let snapshot = if let Some(provided) = body.envelope_snapshot {
        Some(provided)
    } else if active_env.is_some() {
        Some(freeze_resolved_envelope(&conn, &task.id)?)
    } else {
        None
    };

    let created = runs::create(
        &conn,
        runs::CreateInput {
            task_id: &task.id,
            session_id: body.session_id.as_deref(),
            agent: body.agent.as_deref().unwrap_or("main"),
            status: Some(status),
            envelope_id: envelope_id.as_deref(),
            envelope_snapshot: snapshot.as_ref(),
        },
    )?;
    drop(conn);
    if let Some(ref run) = created {
        app.emit(
            "run:created",
            serde_json::json!({ "id": run.id, "task_id": run.task_id, "status": status }),
        );
    }
    json_or_404(created)
}

fn freeze_resolved_envelope(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> ApiResult<Value> {
    const RESOLVE_CHAIN_MAX_DEPTH: i64 = 32;
    let chain = task_envelopes::resolve_chain(conn, task_id, RESOLVE_CHAIN_MAX_DEPTH)?;
    Ok(task_envelopes::resolve_chain_active(&chain))
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Run>> {
    json_or_404(runs::get(&app.conn(), &id)?)
}

async fn start(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Run>> {
    let updated = runs::mark_started(&app.conn(), &id)?;
    if let Some(ref run) = updated {
        app.emit(
            "run:updated",
            serde_json::json!({ "id": run.id, "task_id": run.task_id, "status": "started" }),
        );
    }
    json_or_404(updated)
}

#[derive(Deserialize)]
struct FinishBody {
    result: String,
    notes: Option<String>,
}

async fn finish(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FinishBody>,
) -> ApiResult<Json<Run>> {
    let updated = runs::finish(
        &app.conn(),
        &id,
        &body.result,
        body.notes.as_deref(),
    )?;
    if let Some(ref run) = updated {
        app.emit(
            "run:updated",
            serde_json::json!({
                "id": run.id,
                "task_id": run.task_id,
                "status": "finished",
                "result": body.result,
            }),
        );
    }
    json_or_404(updated)
}

#[cfg(test)]
mod tests {
    //! Verification: `cargo test routes::runs` (RL-U4-08b / LM-176).
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{plans, projects, task_envelopes, tasks, units};
    use crate::routes::runs::router as runs_router;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
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

    struct Fx {
        _dir: tempfile::TempDir,
        app: axum::Router,
        state: AppState,
        task_id: String,
    }

    fn fixture() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("runs_test.db")).unwrap();
        let project = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: None,
                key: None,
            },
        )
        .unwrap()
        .unwrap();
        let plan = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &project.id,
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        plans::approve(&db.conn, &plan.id).unwrap();
        let unit = units::create(
            &db.conn,
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
        let task = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id: &unit.id,
                idx: None,
                title: "T1",
                body: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
                assignee: None,
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
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = runs_router().with_state(state.clone());
        Fx {
            _dir: dir,
            app,
            state,
            task_id: task.id,
        }
    }

    async fn post_runs(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/runs")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn create_started_no_envelope_when_task_unsigned() {
        let fx = fixture();
        let (status, v) = post_runs(&fx.app, json!({"task_id": fx.task_id})).await;
        assert_eq!(status, StatusCode::OK, "body: {v}");
        assert_eq!(v["status"], "started");
        assert!(v.get("envelope_id").map(|x| x.is_null()).unwrap_or(true));
        assert!(v.get("envelope_snapshot").map(|x| x.is_null()).unwrap_or(true));
    }

    #[tokio::test]
    async fn create_pending_freezes_envelope_snapshot() {
        let fx = fixture();
        {
            let mut conn = fx.state.conn();
            let env_json = serde_json::to_string(&json!({
                "intent": "test",
                "prompt_template": "do X",
                "success_criteria": "- pass"
            }))
            .unwrap();
            task_envelopes::sign_for_task(&mut conn, &fx.task_id, &env_json, "main").unwrap();
        }
        let (status, v) =
            post_runs(&fx.app, json!({"task_id": fx.task_id, "status": "pending"})).await;
        assert_eq!(status, StatusCode::OK, "body: {v}");
        assert_eq!(v["status"], "pending");
        assert!(
            v["envelope_id"].is_string(),
            "envelope_id should be set: {v}"
        );
        assert_eq!(v["envelope_snapshot"]["intent"], "test");
        assert_eq!(v["envelope_snapshot"]["prompt_template"], "do X");
    }

    #[tokio::test]
    async fn create_rejects_invalid_status() {
        let fx = fixture();
        let (status, _) =
            post_runs(&fx.app, json!({"task_id": fx.task_id, "status": "bogus"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn start_flips_pending_to_started() {
        let fx = fixture();
        let (_, created) =
            post_runs(&fx.app, json!({"task_id": fx.task_id, "status": "pending"})).await;
        let run_id = created["id"].as_str().unwrap().to_string();
        let resp = fx
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runs/{run_id}/start"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "started");
    }
}
