//! Task lock endpoints (RL-U3-14 / LM-67).
//!
//! - `POST   /tasks/:id/lock`            — acquire / refresh
//! - `DELETE /tasks/:id/lock`            — release
//! - `PATCH  /tasks/:id/lock/heartbeat`  — extend TTL
//! - `GET    /tasks/:id/lock`            — read current holder (diagnostics)
//!
//! Conflict (live lock owned by a different session) responds with HTTP 409
//! and a `Retry-After` header in seconds based on the holder's
//! `expires_at`. Heartbeat / release on a non-owner respond 404 to keep
//! callers honest about ownership.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::id::now_ms;
use crate::models::Task;
use crate::repo::{locks, tasks};
use crate::routes::error::{ApiError, ApiResult};
use crate::state::AppState;

const DEFAULT_TTL_SECONDS: i64 = 600;
const MIN_TTL_SECONDS: i64 = 5;
const MAX_TTL_SECONDS: i64 = 3600 * 6; // 6h ceiling

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/tasks/{id}/lock",
            post(acquire_lock).delete(release_lock).get(get_lock),
        )
        .route("/tasks/{id}/lock/heartbeat", patch(heartbeat_lock))
}

#[derive(Deserialize)]
struct AcquireBody {
    session_id: String,
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct HeartbeatBody {
    session_id: String,
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct ReleaseBody {
    session_id: String,
}

#[derive(Serialize)]
struct LockEnvelope {
    lock: locks::TaskLock,
}

fn validate_ttl(raw: Option<i64>) -> ApiResult<i64> {
    let ttl = raw.unwrap_or(DEFAULT_TTL_SECONDS);
    if !(MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(&ttl) {
        return Err(ApiError::bad_request(format!(
            "ttl_seconds must be between {MIN_TTL_SECONDS} and {MAX_TTL_SECONDS}"
        )));
    }
    Ok(ttl)
}

fn require_task(conn: &rusqlite::Connection, id: &str) -> ApiResult<Task> {
    tasks::get(conn, id)?.ok_or_else(|| ApiError::not_found(format!("Task not found: {id}")))
}

fn require_session(s: &str) -> ApiResult<()> {
    if s.trim().is_empty() {
        return Err(ApiError::bad_request("session_id is required"));
    }
    Ok(())
}

async fn acquire_lock(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AcquireBody>,
) -> ApiResult<axum::response::Response> {
    require_session(&body.session_id)?;
    let ttl_s = validate_ttl(body.ttl_seconds)?;
    let conn = app.conn();
    let task = require_task(&conn, &id)?;
    match locks::acquire(&conn, &task.id, &body.session_id, ttl_s * 1000)? {
        locks::AcquireOutcome::Acquired(lock) => {
            Ok((StatusCode::OK, Json(LockEnvelope { lock })).into_response())
        }
        locks::AcquireOutcome::Conflict(holder) => {
            let retry_after_s = ((holder.expires_at - now_ms()) / 1000).max(1);
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_str(&retry_after_s.to_string()).unwrap(),
            );
            let body = serde_json::json!({
                "error": "Task lock is held by another session",
                "session_id": holder.session_id,
                "expires_at": holder.expires_at,
                "retry_after_seconds": retry_after_s,
            });
            Ok((StatusCode::CONFLICT, headers, Json(body)).into_response())
        }
    }
}

async fn release_lock(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReleaseBody>,
) -> ApiResult<axum::response::Response> {
    require_session(&body.session_id)?;
    let conn = app.conn();
    let task = require_task(&conn, &id)?;
    let released = locks::release(&conn, &task.id, &body.session_id)?;
    if !released {
        return Err(ApiError::not_found(format!(
            "No lock owned by session_id={} on task {}",
            body.session_id, task.id
        )));
    }
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}

async fn heartbeat_lock(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<HeartbeatBody>,
) -> ApiResult<Json<LockEnvelope>> {
    require_session(&body.session_id)?;
    let ttl_s = validate_ttl(body.ttl_seconds)?;
    let conn = app.conn();
    let task = require_task(&conn, &id)?;
    let lock =
        locks::heartbeat(&conn, &task.id, &body.session_id, ttl_s * 1000)?.ok_or_else(|| {
            ApiError::not_found(format!(
                "No live lock owned by session_id={} on task {}",
                body.session_id, task.id
            ))
        })?;
    Ok(Json(LockEnvelope { lock }))
}

async fn get_lock(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<axum::response::Response> {
    let conn = app.conn();
    let task = require_task(&conn, &id)?;
    match locks::get(&conn, &task.id)? {
        Some(lock) => Ok((StatusCode::OK, Json(LockEnvelope { lock })).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans as plans_repo, projects, units};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use serde_json::Value;
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
        // API-TASK-001: cycle_id is mandatory at task creation. Provide a
        // planning cycle and activate it so the HTTP test fixture can satisfy
        // the contract without changing every call site.
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
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    async fn send(
        app: &axum::Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let body_bytes = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let resp = app
            .clone()
            .oneshot(builder.body(body_bytes).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        (status, headers, body_json(resp).await)
    }

    async fn create_task(s: &Setup) -> String {
        let (status, _, body) = send(
            &s.app,
            Method::POST,
            "/tasks",
            Some(serde_json::json!({
                "unit_id": s.unit_id,
                "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "intent": "test",
                    "prompt_template": "test prompt",
                    "success_criteria": ["ok"],
                },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "tasks.create failed: {body:?}");
        body["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn acquire_succeeds_when_no_existing_lock() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (status, _, body) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 60})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["lock"]["session_id"], "sess-A");
    }

    #[tokio::test]
    async fn acquire_returns_409_with_retry_after_when_held_by_other() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (s1, _, _) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 60})),
        )
        .await;
        assert_eq!(s1, StatusCode::OK);
        let (status, headers, body) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-B", "ttl_seconds": 60})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(headers.contains_key("retry-after"));
        let ra: i64 = headers
            .get("retry-after")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(ra > 0 && ra <= 60);
        assert_eq!(body["session_id"], "sess-A");
    }

    #[tokio::test]
    async fn acquire_same_session_refreshes_lock() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (_, _, b1) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 30})),
        )
        .await;
        let (status, _, b2) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 600})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            b2["lock"]["expires_at"].as_i64().unwrap() > b1["lock"]["expires_at"].as_i64().unwrap()
        );
    }

    #[tokio::test]
    async fn heartbeat_extends_ttl_for_owner() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (_, _, before) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 30})),
        )
        .await;
        // Sleep just enough that now_ms moves forward.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let (status, _, after) = send(
            &s.app,
            Method::PATCH,
            &format!("/tasks/{task_id}/lock/heartbeat"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 300})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            after["lock"]["expires_at"].as_i64().unwrap()
                > before["lock"]["expires_at"].as_i64().unwrap()
        );
    }

    #[tokio::test]
    async fn heartbeat_returns_404_for_non_owner() {
        let s = setup();
        let task_id = create_task(&s).await;
        let _ = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 60})),
        )
        .await;
        let (status, _, _) = send(
            &s.app,
            Method::PATCH,
            &format!("/tasks/{task_id}/lock/heartbeat"),
            Some(serde_json::json!({"session_id": "sess-X", "ttl_seconds": 60})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn release_succeeds_only_for_owner_then_lock_is_free() {
        let s = setup();
        let task_id = create_task(&s).await;
        let _ = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 60})),
        )
        .await;
        let (s_other, _, _) = send(
            &s.app,
            Method::DELETE,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-X"})),
        )
        .await;
        assert_eq!(s_other, StatusCode::NOT_FOUND);
        let (s_owner, _, _) = send(
            &s.app,
            Method::DELETE,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A"})),
        )
        .await;
        assert_eq!(s_owner, StatusCode::NO_CONTENT);
        // Now sess-B can take it.
        let (s_b, _, _) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-B", "ttl_seconds": 60})),
        )
        .await;
        assert_eq!(s_b, StatusCode::OK);
    }

    #[tokio::test]
    async fn get_lock_returns_204_when_unlocked() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (status, _, _) =
            send(&s.app, Method::GET, &format!("/tasks/{task_id}/lock"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn get_lock_returns_holder_when_locked() {
        let s = setup();
        let task_id = create_task(&s).await;
        let _ = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 60})),
        )
        .await;
        let (status, _, body) =
            send(&s.app, Method::GET, &format!("/tasks/{task_id}/lock"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["lock"]["session_id"], "sess-A");
    }

    #[tokio::test]
    async fn acquire_rejects_invalid_ttl() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (status, _, _) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "sess-A", "ttl_seconds": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn acquire_rejects_empty_session() {
        let s = setup();
        let task_id = create_task(&s).await;
        let (status, _, _) = send(
            &s.app,
            Method::POST,
            &format!("/tasks/{task_id}/lock"),
            Some(serde_json::json!({"session_id": "  "})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn acquire_unknown_task_returns_404() {
        let s = setup();
        let (status, _, _) = send(
            &s.app,
            Method::POST,
            "/tasks/TASK-NOPE/lock",
            Some(serde_json::json!({"session_id": "sess-A"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
