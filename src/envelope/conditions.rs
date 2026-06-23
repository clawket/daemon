//! ADR-0005 precondition/postcondition evaluator (RL-U3-10 / LM-63).
//!
//! Predicates are JSON objects discriminated by a `type` field. Five
//! kinds are recognised:
//!
//!   - `task_status`     — assert another task's status equals a value
//!     `{ "type":"task_status", "task_id":"TASK-…|TICKET", "equals":"done" }`
//!   - `file_exists`     — assert a file is present on disk (relative to
//!     the envelope's `target_repo` cwd, or absolute)
//!     `{ "type":"file_exists", "path":"src/foo.rs" }`
//!   - `knowledge_exists` — assert at least one knowledge row matches the
//!     filter (`knowledge_type`, `title_contains`, `task_id`)
//!     `{ "type":"knowledge_exists", "knowledge_type":"decision",
//!        "title_contains":"ADR-0005" }`
//!   - `daemon_healthy`  — trivially true at evaluation time (we are
//!     servicing an HTTP request, so the daemon is up)
//!     `{ "type":"daemon_healthy" }`
//!   - `custom_cmd`      — shell out and require the chosen exit
//!     polarity within a bounded timeout
//!     `{ "type":"custom_cmd", "cmd":"cargo", "args":["check"],
//!        "timeout_secs":30, "expect_success":true }`
//!
//! Hooked from `routes::tasks::update`: when a status transition lands
//! on `in_progress` the resolved envelope's `preconditions` array is
//! evaluated; on `done` the `postconditions` array is evaluated.
//! A failing predicate maps to HTTP 409 with the offending JSON
//! attached to the error body's `details` field.

use rusqlite::{params, params_from_iter, Connection};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_CMD_TIMEOUT_SECS: u64 = 10;
const MAX_CMD_TIMEOUT_SECS: u64 = 60;

pub struct EvalContext<'a> {
    pub conn: &'a Connection,
    pub repo_path: Option<PathBuf>,
}

impl<'a> EvalContext<'a> {
    pub fn new(conn: &'a Connection, repo_path: Option<PathBuf>) -> Self {
        Self { conn, repo_path }
    }
}

#[derive(Debug, Clone)]
pub struct Violation {
    pub predicate: Value,
    pub reason: String,
}

/// Evaluate every predicate in order. Returns on the first failure so
/// callers can attach the offending predicate to their error response.
pub fn evaluate(predicates: &[Value], ctx: &EvalContext<'_>) -> Result<(), Violation> {
    for p in predicates {
        eval_one(p, ctx)?;
    }
    Ok(())
}

fn eval_one(pred: &Value, ctx: &EvalContext<'_>) -> Result<(), Violation> {
    let kind = pred
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Violation {
            predicate: pred.clone(),
            reason: "predicate missing `type`".into(),
        })?;
    match kind {
        "task_status" => eval_task_status(pred, ctx),
        "file_exists" => eval_file_exists(pred, ctx),
        "knowledge_exists" => eval_knowledge_exists(pred, ctx),
        "daemon_healthy" => Ok(()),
        "custom_cmd" => eval_custom_cmd(pred, ctx),
        other => Err(Violation {
            predicate: pred.clone(),
            reason: format!("unknown predicate type {:?}", other),
        }),
    }
}

fn eval_task_status(pred: &Value, ctx: &EvalContext<'_>) -> Result<(), Violation> {
    let task_id = pred
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Violation {
            predicate: pred.clone(),
            reason: "task_status: missing `task_id`".into(),
        })?;
    let expected = pred
        .get("equals")
        .and_then(Value::as_str)
        .ok_or_else(|| Violation {
            predicate: pred.clone(),
            reason: "task_status: missing `equals`".into(),
        })?;
    let row: rusqlite::Result<String> = ctx.conn.query_row(
        "SELECT status FROM tasks WHERE id = ?1 OR ticket_number = ?1",
        params![task_id],
        |r| r.get::<_, String>(0),
    );
    match row {
        Ok(actual) if actual == expected => Ok(()),
        Ok(actual) => Err(Violation {
            predicate: pred.clone(),
            reason: format!(
                "task {} status is {:?}, expected {:?}",
                task_id, actual, expected
            ),
        }),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(Violation {
            predicate: pred.clone(),
            reason: format!("task {} not found", task_id),
        }),
        Err(e) => Err(Violation {
            predicate: pred.clone(),
            reason: format!("task_status query error: {}", e),
        }),
    }
}

fn eval_file_exists(pred: &Value, ctx: &EvalContext<'_>) -> Result<(), Violation> {
    let path = pred
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Violation {
            predicate: pred.clone(),
            reason: "file_exists: missing `path`".into(),
        })?;
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        match ctx.repo_path.as_ref() {
            Some(root) => root.join(path),
            None => {
                return Err(Violation {
                    predicate: pred.clone(),
                    reason: format!(
                        "file_exists: cannot resolve relative path {:?} — envelope.target_repo not set or not registered",
                        path
                    ),
                })
            }
        }
    };
    if resolved.exists() {
        Ok(())
    } else {
        Err(Violation {
            predicate: pred.clone(),
            reason: format!("file does not exist: {}", resolved.display()),
        })
    }
}

fn eval_knowledge_exists(pred: &Value, ctx: &EvalContext<'_>) -> Result<(), Violation> {
    let mut sql = String::from("SELECT COUNT(*) FROM knowledge WHERE 1=1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(t) = pred.get("knowledge_type").and_then(Value::as_str) {
        sql.push_str(" AND type = ?");
        args.push(t.to_string().into());
    }
    if let Some(t) = pred.get("title_contains").and_then(Value::as_str) {
        sql.push_str(" AND title LIKE ?");
        args.push(format!("%{}%", t).into());
    }
    if let Some(t) = pred.get("task_id").and_then(Value::as_str) {
        sql.push_str(" AND task_id = ?");
        args.push(t.to_string().into());
    }
    let count: i64 = ctx
        .conn
        .query_row(&sql, params_from_iter(args.iter()), |r| r.get::<_, i64>(0))
        .map_err(|e| Violation {
            predicate: pred.clone(),
            reason: format!("knowledge_exists query error: {}", e),
        })?;
    if count > 0 {
        Ok(())
    } else {
        Err(Violation {
            predicate: pred.clone(),
            reason: "no matching knowledge entry found".into(),
        })
    }
}

fn eval_custom_cmd(pred: &Value, ctx: &EvalContext<'_>) -> Result<(), Violation> {
    let cmd = pred
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| Violation {
            predicate: pred.clone(),
            reason: "custom_cmd: missing `cmd`".into(),
        })?;
    let args: Vec<&str> = pred
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let timeout = pred
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CMD_TIMEOUT_SECS)
        .clamp(1, MAX_CMD_TIMEOUT_SECS);
    let expect_success = pred
        .get("expect_success")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut command = Command::new(cmd);
    command.args(&args);
    if let Some(cwd) = ctx.repo_path.as_ref() {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child: Child = command.spawn().map_err(|e| Violation {
        predicate: pred.clone(),
        reason: format!("custom_cmd spawn failed: {}", e),
    })?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let success = status.success();
                if expect_success == success {
                    return Ok(());
                }
                return Err(Violation {
                    predicate: pred.clone(),
                    reason: format!(
                        "custom_cmd exited with success={} (expect_success={})",
                        success, expect_success
                    ),
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Violation {
                        predicate: pred.clone(),
                        reason: format!("custom_cmd timed out after {}s", timeout),
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(Violation {
                    predicate: pred.clone(),
                    reason: format!("custom_cmd wait failed: {}", e),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, tasks, units};
    use serde_json::json;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Db,
        unit_id: String,
        plan_id: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
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
                auto_advance: false,
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
        Fixture {
            _dir: dir,
            db,
            unit_id: unit.id,
            plan_id: plan.id,
        }
    }

    fn make_task(f: &mut Fixture, title: &str) -> crate::models::Task {
        tasks::create(
            &mut f.db.conn,
            tasks::CreateInput {
                unit_id: &f.unit_id,
                title,
                body: None,
                assignee: None,
                idx: None,
                depends_on: Vec::new(),
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
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
        .unwrap()
    }

    #[test]
    fn evaluate_runs_all_predicates_when_each_passes() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let preds = vec![json!({"type": "daemon_healthy"})];
        evaluate(&preds, &ctx).unwrap();
    }

    fn force_status(conn: &Connection, task_id: &str, status: &str) {
        conn.execute(
            "UPDATE tasks SET status = ?1 WHERE id = ?2",
            params![status, task_id],
        )
        .unwrap();
    }

    #[test]
    fn task_status_passes_when_status_matches() {
        let mut f = fixture();
        let t = make_task(&mut f, "dep");
        force_status(&f.db.conn, &t.id, "done");
        let ctx = EvalContext::new(&f.db.conn, None);
        let pred = json!({"type": "task_status", "task_id": t.id, "equals": "done"});
        evaluate(&[pred], &ctx).unwrap();
    }

    #[test]
    fn task_status_violates_when_status_mismatches() {
        let mut f = fixture();
        let t = make_task(&mut f, "dep");
        let ctx = EvalContext::new(&f.db.conn, None);
        let pred = json!({"type": "task_status", "task_id": t.id, "equals": "done"});
        let v = evaluate(std::slice::from_ref(&pred), &ctx).unwrap_err();
        assert_eq!(v.predicate, pred);
        assert!(v.reason.contains("expected"));
    }

    #[test]
    fn task_status_violates_when_task_missing() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let pred = json!({"type": "task_status", "task_id": "TASK-NOPE", "equals": "done"});
        let v = evaluate(&[pred], &ctx).unwrap_err();
        assert!(v.reason.contains("not found"));
    }

    #[test]
    fn file_exists_passes_for_present_relative_path() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let ctx = EvalContext::new(&f.db.conn, Some(dir.path().to_path_buf()));
        evaluate(&[json!({"type": "file_exists", "path": "README.md"})], &ctx).unwrap();
    }

    #[test]
    fn file_exists_violates_when_missing() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let ctx = EvalContext::new(&f.db.conn, Some(dir.path().to_path_buf()));
        let v = evaluate(&[json!({"type": "file_exists", "path": "nope.txt"})], &ctx).unwrap_err();
        assert!(v.reason.contains("does not exist"));
    }

    #[test]
    fn file_exists_violates_when_relative_with_no_repo_path() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(
            &[json!({"type": "file_exists", "path": "src/foo.rs"})],
            &ctx,
        )
        .unwrap_err();
        assert!(v.reason.contains("target_repo"));
    }

    #[test]
    fn knowledge_exists_passes_when_row_matches_filters() {
        let f = fixture();
        crate::repo::knowledge::create(
            &f.db.conn,
            crate::repo::knowledge::CreateInput {
                task_id: None,
                unit_id: None,
                plan_id: Some(&f.plan_id),
                type_: "decision",
                title: "ADR-0005 condition DSL",
                content: Some("body"),
                content_format: Some("markdown"),
                parent_id: None,
            },
        )
        .unwrap()
        .unwrap();
        let ctx = EvalContext::new(&f.db.conn, None);
        evaluate(
            &[json!({
                "type": "knowledge_exists",
                "knowledge_type": "decision",
                "title_contains": "ADR-0005"
            })],
            &ctx,
        )
        .unwrap();
    }

    #[test]
    fn knowledge_exists_violates_when_no_match() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(
            &[json!({"type": "knowledge_exists", "knowledge_type": "decision"})],
            &ctx,
        )
        .unwrap_err();
        assert!(v.reason.contains("no matching"));
    }

    #[test]
    fn daemon_healthy_always_passes() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        evaluate(&[json!({"type": "daemon_healthy"})], &ctx).unwrap();
    }

    #[test]
    fn custom_cmd_passes_when_exit_zero_expected() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        evaluate(
            &[json!({"type": "custom_cmd", "cmd": "true", "timeout_secs": 5})],
            &ctx,
        )
        .unwrap();
    }

    #[test]
    fn custom_cmd_violates_when_unexpected_exit() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(
            &[json!({
                "type": "custom_cmd",
                "cmd": "false",
                "timeout_secs": 5,
                "expect_success": true
            })],
            &ctx,
        )
        .unwrap_err();
        assert!(v.reason.contains("success="));
    }

    #[test]
    fn custom_cmd_inverted_polarity_passes_on_nonzero_exit() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        evaluate(
            &[json!({
                "type": "custom_cmd",
                "cmd": "false",
                "timeout_secs": 5,
                "expect_success": false
            })],
            &ctx,
        )
        .unwrap();
    }

    #[test]
    fn custom_cmd_times_out_on_long_running_command() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(
            &[json!({
                "type": "custom_cmd",
                "cmd": "sleep",
                "args": ["3"],
                "timeout_secs": 1
            })],
            &ctx,
        )
        .unwrap_err();
        assert!(v.reason.contains("timed out"));
    }

    #[test]
    fn unknown_predicate_type_violates() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(&[json!({"type": "telemetry_burp"})], &ctx).unwrap_err();
        assert!(v.reason.contains("unknown predicate type"));
    }

    #[test]
    fn missing_type_violates() {
        let f = fixture();
        let ctx = EvalContext::new(&f.db.conn, None);
        let v = evaluate(&[json!({"path": "/tmp"})], &ctx).unwrap_err();
        assert!(v.reason.contains("missing"));
    }

    #[test]
    fn evaluate_short_circuits_on_first_failure() {
        let mut f = fixture();
        let t = make_task(&mut f, "dep");
        let ctx = EvalContext::new(&f.db.conn, None);
        let preds = vec![
            json!({"type": "task_status", "task_id": t.id, "equals": "done"}),
            json!({"type": "daemon_healthy"}),
        ];
        let v = evaluate(&preds, &ctx).unwrap_err();
        assert_eq!(v.predicate.get("type").unwrap(), "task_status");
    }
}
