//! Token usage / cost endpoints (RL-U3-12 / LM-65).
//!
//! - `POST /tasks/:id/usage`        — record one run's usage
//! - `GET  /tasks/:id/usage`        — list runs + totals + budget summary
//! - `GET  /tasks/:id/usage/preflight` — 200 when under budget, 429 when over;
//!   intended for PreToolUse hook gating per ADR-0007.
//! - `GET  /plans/:id/usage`        — model-grouped aggregate across the plan
//!
//! Budget is read from the resolved envelope's `token_budget` field
//! (deep-merged through the inheritance chain, so a plan-level budget on
//! a parent task flows down to every descendant unless overridden).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::repo::{plans, task_envelopes, tasks, usage};
use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

const RESOLVE_CHAIN_MAX_DEPTH: i64 = 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/tasks/{id}/usage",
            get(list_task_usage).post(record_task_usage),
        )
        .route("/tasks/{id}/usage/preflight", get(preflight_task_usage))
        .route("/plans/{id}/usage", get(list_plan_usage))
}

#[derive(Deserialize)]
struct RecordBody {
    run_id: String,
    model: String,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
    #[serde(default)]
    cost_usd: f64,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    ended_at: Option<i64>,
    /// Optional snapshot — defaults to the live envelope's `token_budget`.
    #[serde(default)]
    token_budget_snapshot: Option<Value>,
}

#[derive(Serialize)]
struct UsageResponse {
    runs: Vec<usage::UsageRecord>,
    totals: usage::Totals,
    budget: BudgetReport,
}

#[derive(Serialize)]
struct BudgetReport {
    /// `null` when the resolved envelope has no `token_budget` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<Value>,
    spent: usage::Totals,
    over_input: bool,
    over_output: bool,
    over_cost: bool,
    /// `true` when any of the three metrics exceeds its budget.
    exceeded: bool,
}

async fn record_task_usage(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RecordBody>,
) -> ApiResult<Json<usage::UsageRecord>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    if body.run_id.trim().is_empty() {
        return Err(ApiError::bad_request("run_id is required"));
    }
    if body.model.trim().is_empty() {
        return Err(ApiError::bad_request("model is required"));
    }
    // Default the snapshot to the active envelope's token_budget so
    // retroactive budget edits don't rewrite history.
    let snapshot = match body.token_budget_snapshot {
        Some(v) => Some(v.to_string()),
        None => resolved_token_budget(&conn, &task.id)?
            .as_ref()
            .map(Value::to_string),
    };
    let rec = usage::record(
        &conn,
        usage::RecordInput {
            run_id: &body.run_id,
            task_id: &task.id,
            model: &body.model,
            input_tokens: body.input_tokens,
            output_tokens: body.output_tokens,
            cache_read_tokens: body.cache_read_tokens,
            cache_write_tokens: body.cache_write_tokens,
            cost_usd: body.cost_usd,
            started_at: body.started_at,
            ended_at: body.ended_at,
            token_budget_snapshot: snapshot.as_deref(),
        },
    )?;
    Ok(Json(rec))
}

async fn list_task_usage(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<UsageResponse>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let runs = usage::list_by_task(&conn, &task.id)?;
    let totals = usage::task_total(&conn, &task.id)?;
    let budget_value = resolved_token_budget(&conn, &task.id)?;
    let budget = build_budget_report(budget_value, &totals);
    Ok(Json(UsageResponse {
        runs,
        totals,
        budget,
    }))
}

async fn preflight_task_usage(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let totals = usage::task_total(&conn, &task.id)?;
    let budget_value = resolved_token_budget(&conn, &task.id)?;
    let report = build_budget_report(budget_value, &totals);
    if report.exceeded {
        // 429 Too Many Requests — the closest standard status to "you've
        // burned through your budget, back off". The hook treats this as
        // a hard stop until the budget is bumped.
        let body = serde_json::json!({
            "error": "token_budget exceeded",
            "details": report,
        });
        return Ok((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
    }
    Ok((StatusCode::OK, Json(serde_json::json!(report))).into_response())
}

async fn list_plan_usage(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<PlanUsageResponse>> {
    let conn = app.conn();
    let plan = plans::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Plan not found: {}", id)))?;
    let by_model = usage::aggregate_by_plan(&conn, &plan.id)?;
    let mut combined = usage::Totals::default();
    for m in &by_model {
        combined.input_tokens += m.totals.input_tokens;
        combined.output_tokens += m.totals.output_tokens;
        combined.cache_read_tokens += m.totals.cache_read_tokens;
        combined.cache_write_tokens += m.totals.cache_write_tokens;
        combined.cost_usd += m.totals.cost_usd;
        combined.run_count += m.totals.run_count;
    }
    Ok(Json(PlanUsageResponse {
        by_model,
        totals: combined,
    }))
}

#[derive(Serialize)]
struct PlanUsageResponse {
    by_model: Vec<usage::ModelAggregate>,
    totals: usage::Totals,
}

/// Read the `token_budget` from the resolved envelope chain (parent →
/// self deep-merged). Returns `None` if no level supplies one.
fn resolved_token_budget(conn: &rusqlite::Connection, task_id: &str) -> ApiResult<Option<Value>> {
    let chain = task_envelopes::resolve_chain(conn, task_id, RESOLVE_CHAIN_MAX_DEPTH)?;
    if chain.is_empty() {
        return Ok(None);
    }
    let mut budget: Option<Value> = None;
    for entry in &chain {
        let Some(json) = &entry.json else { continue };
        let Ok(env) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        if let Some(b) = env.get("token_budget") {
            // Deeper level wins.
            budget = Some(b.clone());
        }
    }
    Ok(budget)
}

fn build_budget_report(budget: Option<Value>, totals: &usage::Totals) -> BudgetReport {
    let (over_input, over_output, over_cost) = match budget.as_ref() {
        Some(Value::Object(o)) => (
            o.get("input_tokens")
                .and_then(Value::as_i64)
                .map(|cap| totals.input_tokens > cap)
                .unwrap_or(false),
            o.get("output_tokens")
                .and_then(Value::as_i64)
                .map(|cap| totals.output_tokens > cap)
                .unwrap_or(false),
            o.get("cost_usd")
                .and_then(Value::as_f64)
                .map(|cap| totals.cost_usd > cap)
                .unwrap_or(false),
        ),
        _ => (false, false, false),
    };
    BudgetReport {
        budget,
        spent: totals.clone(),
        over_input,
        over_output,
        over_cost,
        exceeded: over_input || over_output || over_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans as plans_repo, projects, units};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
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

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        plan_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        let plan = plans_repo::create(
            &db.conn,
            plans_repo::CreateInput {
                project_id: &project.id,
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        plans_repo::approve(&db.conn, &plan.id).unwrap();
        let unit = units::create(
            &db.conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: "U",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        let cycle = cycles::create(
            &db.conn,
            cycles::CreateInput {
                project_id: &project.id,
                unit_id: &unit.id,
                title: "C1",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = crate::routes::router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            plan_id: plan.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let s = resp.status();
        (s, body_json(resp).await)
    }

    async fn get_uri(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let s = resp.status();
        (s, body_json(resp).await)
    }

    async fn create_task(s: &Setup, envelope: Option<Value>) -> String {
        let mut body = serde_json::json!({
            "unit_id": s.unit_id,
            "cycle_id": s.cycle_id,
            "title": "T",
        });
        if let Some(env) = envelope {
            body["envelope"] = env;
        }
        let (status, body) = post(&s.app, "/tasks", body).await;
        assert_eq!(status, StatusCode::OK, "tasks.create failed: {:?}", body);
        body["id"].as_str().unwrap().to_string()
    }

    async fn create_run(s: &Setup, task_id: &str) -> String {
        let body = serde_json::json!({"task_id": task_id, "agent": "main"});
        let (status, body) = post(&s.app, "/runs", body).await;
        assert_eq!(status, StatusCode::OK, "runs.create failed: {:?}", body);
        body["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn record_then_list_round_trip() {
        let s = setup();
        let task_id = create_task(&s, None).await;
        let run_id = create_run(&s, &task_id).await;
        let (status, _) = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({
                "run_id": run_id,
                "model": "claude-opus-4-7",
                "input_tokens": 1000,
                "output_tokens": 500,
                "cost_usd": 1.5,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = get_uri(&s.app, &format!("/tasks/{}/usage", task_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["runs"].as_array().unwrap().len(), 1);
        assert_eq!(body["totals"]["input_tokens"], 1000);
        assert_eq!(body["totals"]["run_count"], 1);
        assert_eq!(body["budget"]["exceeded"], false);
    }

    #[tokio::test]
    async fn preflight_returns_429_when_budget_exceeded() {
        let s = setup();
        let task_id = create_task(
            &s,
            Some(serde_json::json!({
                "version": 1,
                "intent": "p",
                "token_budget": {"input_tokens": 100, "output_tokens": 100, "cost_usd": 0.5},
            })),
        )
        .await;
        let run_id = create_run(&s, &task_id).await;
        let (rec_status, _) = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({
                "run_id": run_id,
                "model": "x",
                "input_tokens": 200, // over budget (cap 100)
                "output_tokens": 50,
                "cost_usd": 0.1,
            }),
        )
        .await;
        assert_eq!(rec_status, StatusCode::OK);
        let (status, body) = get_uri(&s.app, &format!("/tasks/{}/usage/preflight", task_id)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"], "token_budget exceeded");
        assert_eq!(body["details"]["over_input"], true);
        assert_eq!(body["details"]["exceeded"], true);
    }

    #[tokio::test]
    async fn preflight_returns_200_when_under_budget() {
        let s = setup();
        let task_id = create_task(
            &s,
            Some(serde_json::json!({
                "version": 1,
                "intent": "p",
                "token_budget": {"input_tokens": 1000, "cost_usd": 10},
            })),
        )
        .await;
        let run_id = create_run(&s, &task_id).await;
        let _ = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({
                "run_id": run_id,
                "model": "x",
                "input_tokens": 100,
                "output_tokens": 50,
                "cost_usd": 0.5,
            }),
        )
        .await;
        let (status, body) = get_uri(&s.app, &format!("/tasks/{}/usage/preflight", task_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["exceeded"], false);
    }

    #[tokio::test]
    async fn preflight_200_when_no_budget_set() {
        let s = setup();
        let task_id = create_task(&s, None).await;
        let (status, body) = get_uri(&s.app, &format!("/tasks/{}/usage/preflight", task_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["exceeded"], false);
    }

    #[tokio::test]
    async fn plan_aggregate_groups_by_model() {
        let s = setup();
        let task_id = create_task(&s, None).await;
        // RUN_ALREADY_OPEN: at most one open run per task. Finish r1 before
        // starting r2 so the aggregation has two distinct runs.
        let r1 = create_run(&s, &task_id).await;
        let _ = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({"run_id": r1, "model": "opus", "input_tokens": 100}),
        )
        .await;
        let _ = post(
            &s.app,
            &format!("/runs/{}/finish", r1),
            serde_json::json!({"result": "success"}),
        )
        .await;
        let r2 = create_run(&s, &task_id).await;
        let _ = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({"run_id": r2, "model": "haiku", "input_tokens": 50}),
        )
        .await;
        let (status, body) = get_uri(&s.app, &format!("/plans/{}/usage", s.plan_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["by_model"].as_array().unwrap().len(), 2);
        assert_eq!(body["totals"]["input_tokens"], 150);
        assert_eq!(body["totals"]["run_count"], 2);
    }

    #[tokio::test]
    async fn record_rejects_empty_run_id() {
        let s = setup();
        let task_id = create_task(&s, None).await;
        let (status, _) = post(
            &s.app,
            &format!("/tasks/{}/usage", task_id),
            serde_json::json!({"run_id": "", "model": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn record_unknown_task_returns_404() {
        let s = setup();
        let (status, _) = post(
            &s.app,
            "/tasks/TASK-NOPE/usage",
            serde_json::json!({"run_id": "RUN-X", "model": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
