// End-to-end HTTP parity tests against the real clawketd binary.
// Each test starts the binary on a random port against a fresh SQLite DB,
// exercises the route, and tears down.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct DaemonHandle {
    child: Child,
    base_url: String,
    token: String,
    _tmpdir: tempfile::TempDir,
}

impl DaemonHandle {
    async fn spawn() -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let db_path = tmpdir.path().join("test.sqlite");
        let bin = env!("CARGO_BIN_EXE_clawketd");

        // Random port via --port 0; we'll discover via the port file.
        let port_file = tmpdir.path().join("port");
        let cache_dir = tmpdir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let child = Command::new(bin)
            .arg("--port")
            .arg("0")
            .arg("--db")
            .arg(&db_path)
            .env("CLAWKET_DATA_DIR", tmpdir.path().join("data"))
            .env("CLAWKET_CACHE_DIR", &cache_dir)
            .env("CLAWKET_CONFIG_DIR", tmpdir.path().join("config"))
            .env("CLAWKET_STATE_DIR", tmpdir.path().join("state"))
            // Pin to a nonexistent path so resolve_web_dir() can't fall back
            // to a sibling workspace web/dist (test was failing when run from
            // a tree that already had web/ built).
            .env("CLAWKET_WEB_DIR", tmpdir.path().join("no-web"))
            .env("CLAWKETD_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn clawketd");

        // The port file is written by daemon to $CACHE/clawketd.port
        let _ = port_file;
        let pf = cache_dir.join("clawketd.port");
        let mut port: Option<u16> = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(s) = std::fs::read_to_string(&pf) {
                if let Ok(p) = s.trim().parse::<u16>() {
                    port = Some(p);
                    break;
                }
            }
        }
        let port = port.expect("daemon port file not written");
        let base_url = format!("http://127.0.0.1:{port}");

        // FIX-DAEMON-108: read the TCP auth token written by the daemon to
        // {cache}/clawketd.token. Required on every non-/health request.
        let token_file = cache_dir.join("clawketd.token");
        let mut token: Option<String> = None;
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&token_file) {
                let t = s.trim().to_string();
                if !t.is_empty() {
                    token = Some(t);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let token = token.expect("daemon token file not written");

        // Wait for health (exempt from token auth).
        let probe = reqwest::Client::new();
        for _ in 0..30 {
            if probe.get(format!("{base_url}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Self {
            child,
            base_url,
            token,
            _tmpdir: tmpdir,
        }
    }

    /// Build a reqwest client that injects the `X-Clawket-Token` header on
    /// every request. Mirrors how the production CLI authenticates against the
    /// TCP listener (FIX-DAEMON-108).
    fn client(&self) -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-clawket-token",
            reqwest::header::HeaderValue::from_str(&self.token).expect("token header value"),
        );
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("build reqwest client")
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoint_returns_ok() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp: serde_json::Value = client
        .get(format!("{}/health", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["engine"], "rust");
    // U6-T3 parity — pid and uptime_ms exposed.
    assert!(resp["pid"].is_u64(), "/health should include pid");
    assert!(resp["pid"].as_u64().unwrap() > 0);
    assert!(
        resp["uptime_ms"].is_u64(),
        "/health should include uptime_ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_writes_pid_file_on_boot() {
    let d = DaemonHandle::spawn().await;
    // pid file lives at {CLAWKET_CACHE_DIR}/clawketd.pid per paths.rs.
    let pid_file = d._tmpdir.path().join("cache").join("clawketd.pid");
    let contents =
        std::fs::read_to_string(&pid_file).expect("pid file should exist after daemon boot");
    let pid: u32 = contents.trim().parse().expect("pid file should hold u32");
    assert!(pid > 0);

    // /health.pid should match the pid file.
    let client = d.client();
    let resp: serde_json::Value = client
        .get(format!("{}/health", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["pid"].as_u64().unwrap() as u32, pid);
}

// LM-69 / ADR-0010: /activity/stats exposes the activity_log retention budget
// snapshot used by `clawket doctor`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_stats_endpoint_returns_budget_snapshot() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp: serde_json::Value = client
        .get(format!("{}/activity/stats", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Default policy values surface verbatim.
    assert_eq!(resp["hot_days"], 90);
    assert_eq!(resp["total_days"], 365);
    assert_eq!(resp["max_mb"], 500);
    assert_eq!(resp["max_bytes"].as_i64().unwrap(), 500 * 1024 * 1024);

    // Fresh DB has no archive batches and no hot rows yet.
    assert_eq!(resp["hot_rows"], 0);
    assert_eq!(resp["archive_batches"], 0);
    assert!(resp["archive_oldest_period_start"].is_null());

    // used_bytes is i64, defaults to 0 on an empty DB.
    assert!(resp["used_bytes"].is_i64());
    assert_eq!(resp["used_bytes"].as_i64().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_project_plan_task_flow() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();

    // Create project
    let project: serde_json::Value = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "itest-proj", "key": "ITP"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();
    assert!(project_id.starts_with("PROJ-"));

    // Create plan
    let plan: serde_json::Value = client
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({"project_id": project_id, "title": "itest plan"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();

    // Approve plan so tasks are creatable
    let _ = client
        .post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    // Create unit (cycle requires unit_id per FIX-DAEMON-002 / PDD A4)
    let unit: serde_json::Value = client
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "U1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unit_id = unit["id"].as_str().unwrap().to_string();

    // Create cycle (Cycle ⊂ Unit) and activate (tasks need active cycle)
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({
            "project_id": project_id,
            "unit_id": unit_id,
            "title": "itest cycle"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cycle_id = cycle["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/cycles/{}/activate", d.base_url, cycle_id))
        .send()
        .await
        .unwrap();

    // Create task (API-TASK-001: cycle_id is required)
    let task: serde_json::Value = client
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": unit_id,
            "cycle_id": cycle_id,
            "title": "Hello task",
            "envelope": {
                "intent": "hello",
                "prompt_template": "hello prompt",
                "success_criteria": ["hello ok"],
            },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = task["id"].as_str().unwrap().to_string();
    assert!(task_id.starts_with("TASK-"));
    assert_eq!(task["title"], "Hello task");

    // List tasks
    let list: Vec<serde_json::Value> = client
        .get(format!("{}/tasks?unit_id={}", d.base_url, unit_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);

    // Patch task status
    let patched: serde_json::Value = client
        .patch(format!("{}/tasks/{}", d.base_url, task_id))
        .json(&serde_json::json!({"status": "in_progress"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(patched["status"], "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_endpoint_returns_array() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp: Vec<String> = client
        .get(format!("{}/agents", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dashboard_empty_when_no_project() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp: serde_json::Value = client
        .get(format!("{}/dashboard", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["project"], serde_json::Value::Null);
    assert_eq!(resp["context"], "");
}

/// Migration 027: GET /continuation drives the Stop-hook auto-advance.
/// Verifies the actionable walk (next todo task → next todo task) and the
/// opt-in gate (auto_advance=0 ⇒ always null).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_reports_next_actionable_for_auto_advance_plan() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let cwd = "/tmp/itest-continuation-aa";

    // Project bound to a cwd so /continuation resolves it explicitly.
    let project: serde_json::Value = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "cont-aa", "key": "CAA", "cwd": cwd}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();

    // Plan with auto_advance=true, approved.
    let plan: serde_json::Value = client
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({
            "project_id": project_id, "title": "cont plan", "auto_advance": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    assert_eq!(plan["auto_advance"], true);
    let _ = client
        .post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    // Unit + active cycle.
    let unit: serde_json::Value = client
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "U1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unit_id = unit["id"].as_str().unwrap().to_string();
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({
            "project_id": project_id, "unit_id": unit_id, "title": "C1"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cycle_id = cycle["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/cycles/{}/activate", d.base_url, cycle_id))
        .send()
        .await
        .unwrap();

    // Two todo tasks (idx 0, idx 1).
    let mk_task = |title: &'static str| {
        let client = client.clone();
        let base = d.base_url.clone();
        let unit_id = unit_id.clone();
        let cycle_id = cycle_id.clone();
        async move {
            let t: serde_json::Value = client
                .post(format!("{base}/tasks"))
                .json(&serde_json::json!({
                    "unit_id": unit_id,
                    "cycle_id": cycle_id,
                    "title": title,
                    "envelope": {
                        "intent": title,
                        "prompt_template": "p",
                        "success_criteria": ["ok"],
                    },
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            t["id"].as_str().unwrap().to_string()
        }
    };
    let t1 = mk_task("T1").await;
    let _t2 = mk_task("T2").await;

    // (1) /continuation → next is the first todo task (T1).
    let resp: serde_json::Value = client
        .get(format!("{}/continuation?cwd={}", d.base_url, cwd))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["next"]["kind"], "task");
    assert_eq!(resp["next"]["id"], t1);
    assert_eq!(resp["next"]["title"], "T1");
    assert!(resp["instruction"].as_str().unwrap().contains("T1"));

    // (2) Cancel T1 (terminal, no evidence needed) → next is T2.
    let _ = client
        .patch(format!("{}/tasks/{}", d.base_url, t1))
        .json(&serde_json::json!({"status": "cancelled"}))
        .send()
        .await
        .unwrap();
    let resp2: serde_json::Value = client
        .get(format!("{}/continuation?cwd={}", d.base_url, cwd))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp2["next"]["kind"], "task");
    assert_eq!(resp2["next"]["title"], "T2");
}

/// Migration 027: a plan with auto_advance=0 must never drive auto-advance —
/// /continuation returns `{ next: null }` even with pending todo tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_null_when_auto_advance_disabled() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let cwd = "/tmp/itest-continuation-off";

    let project: serde_json::Value = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "cont-off", "key": "COF", "cwd": cwd}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let project_id = project["id"].as_str().unwrap().to_string();

    // Plan WITHOUT auto_advance (defaults false).
    let plan: serde_json::Value = client
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({"project_id": project_id, "title": "off plan"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    assert_eq!(plan["auto_advance"], false);
    let _ = client
        .post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    let unit: serde_json::Value = client
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "U1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unit_id = unit["id"].as_str().unwrap().to_string();
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({
            "project_id": project_id, "unit_id": unit_id, "title": "C1"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cycle_id = cycle["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/cycles/{}/activate", d.base_url, cycle_id))
        .send()
        .await
        .unwrap();
    let _t: serde_json::Value = client
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": unit_id,
            "cycle_id": cycle_id,
            "title": "T1",
            "envelope": {"intent": "i", "prompt_template": "p", "success_criteria": ["ok"]},
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let resp: serde_json::Value = client
        .get(format!("{}/continuation?cwd={}", d.base_url, cwd))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["next"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_returns_no_project_message() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp: serde_json::Value = client
        .get(format!("{}/handoff", d.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["content"], "# No project found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiki_files_scans_docs_directory() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    std::fs::write(tmp.path().join("docs/a.md"), "# First doc\n\nbody").unwrap();
    std::fs::write(tmp.path().join("README.md"), "# Readme\n\nhi").unwrap();

    let url = format!(
        "{}/wiki/files?cwd={}",
        d.base_url,
        tmp.path().to_string_lossy()
    );
    let resp: Vec<serde_json::Value> = client.get(url).send().await.unwrap().json().await.unwrap();
    assert!(resp.iter().any(|v| v["title"] == "First doc"));
    assert!(resp.iter().any(|v| v["title"] == "Readme"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plans_import_dryrun_parses_markdown() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let tmp = tempfile::tempdir().unwrap();
    let plan_md = tmp.path().join("plan.md");
    std::fs::write(
        &plan_md,
        "# Test Plan\n\n## Unit 1: First\n\n### T1 foo\n\n### T2 bar\n",
    )
    .unwrap();

    let resp: serde_json::Value = client
        .post(format!("{}/plans/import", d.base_url))
        .json(&serde_json::json!({
            "file": plan_md.to_string_lossy(),
            "cwd": tmp.path().to_string_lossy(),
            "dryRun": true,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["dry_run"], true);
    assert_eq!(resp["plan_title"], "Test Plan");
    assert_eq!(resp["unit_count"], 1);
    assert_eq!(resp["task_count"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn envelope_validate_route_evaluates_draft_envelope() {
    // LM-151 / RL-U6-02 — POST /tasks/:id/envelope/validate accepts a
    // draft envelope (un-saved) and returns the same shape MCP's
    // clawket_validate_envelope does. This is the SSoT for envelope
    // validation now; the web form polls it for real-time feedback.
    let d = DaemonHandle::spawn().await;
    let client = d.client();

    let project: serde_json::Value = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "envval", "key": "EV"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    let plan: serde_json::Value = client
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    let unit: serde_json::Value = client
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "u"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unit_id = unit["id"].as_str().unwrap().to_string();

    // Active cycle is required for task creation (API-TASK-001 + PDD A4).
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "unit_id": unit_id, "title": "c"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cycle_id = cycle["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/cycles/{}/activate", d.base_url, cycle_id))
        .send()
        .await
        .unwrap();

    let task: serde_json::Value = client
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": unit_id,
            "cycle_id": cycle_id,
            "title": "t",
            "envelope": {
                "intent": "validate route test",
                "prompt_template": "validate route test prompt",
                "success_criteria": ["ok"],
            },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = task["id"].as_str().unwrap().to_string();

    // Empty draft → required-field errors.
    let empty_resp: serde_json::Value = client
        .post(format!(
            "{}/tasks/{}/envelope/validate",
            d.base_url, task_id
        ))
        .json(&serde_json::json!({"envelope": {}, "strict": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty_resp["valid"], false);
    let viols = empty_resp["violations"]
        .as_array()
        .expect("violations array");
    let fields: Vec<&str> = viols.iter().filter_map(|v| v["field"].as_str()).collect();
    assert!(fields.contains(&"intent"));
    assert!(fields.contains(&"prompt_template"));
    assert!(fields.contains(&"success_criteria"));
    assert_eq!(empty_resp["evaluated_envelope"], serde_json::json!({}));

    // Valid draft → no errors.
    let good_resp: serde_json::Value = client
        .post(format!(
            "{}/tasks/{}/envelope/validate",
            d.base_url, task_id
        ))
        .json(&serde_json::json!({
            "envelope": {
                "intent": "x",
                "prompt_template": "y",
                "success_criteria": ["it works"],
                "atomic_size_hint": "small",
                "decomposition_policy": "auto",
                "target_model": "sonnet",
                "max_turns": 30,
            },
            "strict": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(good_resp["valid"], true);
    let warn_count = good_resp["violations"]
        .as_array()
        .map(|a| a.iter().filter(|v| v["severity"] == "warning").count())
        .unwrap_or(99);
    assert_eq!(
        warn_count, 0,
        "no warnings expected on fully-populated envelope: {:?}",
        good_resp
    );

    // Bad atomic_size_hint surfaces with that exact field name.
    let bad_size: serde_json::Value = client
        .post(format!(
            "{}/tasks/{}/envelope/validate",
            d.base_url, task_id
        ))
        .json(&serde_json::json!({
            "envelope": {
                "intent": "x",
                "prompt_template": "y",
                "success_criteria": ["z"],
                "atomic_size_hint": "huge",
            },
            "strict": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bad_size["valid"], false);
    let bad_fields: Vec<&str> = bad_size["violations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["field"].as_str())
        .collect();
    assert!(bad_fields.contains(&"atomic_size_hint"));

    // No envelope body + no active envelope → 404. Since v3.0 task
    // creation requires an envelope, drop the active one first to
    // reach the no-envelope state the route must handle.
    let _ = client
        .delete(format!("{}/tasks/{}/envelope", d.base_url, task_id))
        .send()
        .await
        .unwrap();
    let missing = client
        .post(format!(
            "{}/tasks/{}/envelope/validate",
            d.base_url, task_id
        ))
        .json(&serde_json::json!({"strict": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decompose_route_returns_one_suggestion_per_success_criterion() {
    // LM-87 / RL-U6-04 — POST /tasks/:id/decompose. Exercises the
    // daemon-side suggester so the web SuggestionPanel and CLI MCP
    // both land on the same wire shape.
    let d = DaemonHandle::spawn().await;
    let client = d.client();

    let project: serde_json::Value = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "decomp", "key": "DC"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    let plan: serde_json::Value = client
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    let unit: serde_json::Value = client
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "u"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unit_id = unit["id"].as_str().unwrap().to_string();

    // Active cycle is required for task creation (API-TASK-001 + PDD A4).
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "unit_id": unit_id, "title": "c"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cycle_id = cycle["id"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{}/cycles/{}/activate", d.base_url, cycle_id))
        .send()
        .await
        .unwrap();

    let task: serde_json::Value = client
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": unit_id,
            "cycle_id": cycle_id,
            "title": "Parent task",
            "envelope": {
                "intent": "build the thing",
                "prompt_template": "do it",
                "success_criteria": ["criterion A", "criterion B", "criterion C"],
                "decomposition_policy": "auto",
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = task["id"].as_str().unwrap().to_string();

    let resp: serde_json::Value = client
        .post(format!("{}/tasks/{}/decompose", d.base_url, task_id))
        .json(&serde_json::json!({"strategy": "auto", "max_depth": 2}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["parent"]["id"], task_id);
    assert_eq!(resp["strategy"], "auto");
    assert_eq!(resp["max_depth"], 2);
    let suggestions = resp["suggested_subtasks"]
        .as_array()
        .expect("suggestions array");
    assert_eq!(suggestions.len(), 3);
    assert!(suggestions[0]["title"]
        .as_str()
        .unwrap()
        .contains("Parent task"));
    assert!(suggestions[0]["title"]
        .as_str()
        .unwrap()
        .contains("criterion A"));

    // Unknown task → 404.
    let missing = client
        .post(format!("{}/tasks/TASK-NOPE/decompose", d.base_url))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_root_returns_503_when_no_web_dir() {
    let d = DaemonHandle::spawn().await;
    let client = d.client();
    let resp = client.get(format!("{}/", d.base_url)).send().await.unwrap();
    // No web/dist built during test runs → dashboardNotBuilt fallback
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// LM-10833: SPA cookie bootstrap + Origin guard for the TCP auth middleware.
// ---------------------------------------------------------------------------

/// Cookie channel: a request authenticated via `clawket_session` cookie alone
/// (no `X-Clawket-Token` header) is accepted on safe (GET) requests. This is
/// the path the browser SPA uses after the daemon issues the bootstrap cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_only_request_authorized_for_safe_method() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/projects", d.base_url))
        .header("cookie", format!("clawket_session={}", d.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// Cookie + matching Origin: state-changing request with a same-origin
/// `Origin` header passes the CSRF guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_with_same_origin_allowed_for_mutating_method() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let host = d.base_url.trim_start_matches("http://");
    let resp = bare
        .post(format!("{}/projects", d.base_url))
        .header("cookie", format!("clawket_session={}", d.token))
        .header("origin", format!("http://{host}"))
        .json(&serde_json::json!({"name": "lm-10833-cookie-origin-ok"}))
        .send()
        .await
        .unwrap();
    // 200 (created) — anything other than 401 means the auth layer accepted it
    assert_ne!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Cookie + foreign Origin: state-changing cookie request with a foreign
/// `Origin` header is rejected with 401 + ORIGIN_MISMATCH. The cookie alone
/// would be valid; the Origin guard is what rejects it (CSRF defense).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_with_foreign_origin_rejected_on_mutating_method() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/projects", d.base_url))
        .header("cookie", format!("clawket_session={}", d.token))
        .header("origin", "http://evil.example")
        .json(&serde_json::json!({"name": "lm-10833-cookie-origin-rejected"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "ORIGIN_MISMATCH");
}

/// Header channel regression: the `X-Clawket-Token` channel still works for
/// mutating methods *without* an Origin header. The CLI uses this path and
/// must not be broken by the new cookie / Origin logic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn header_channel_mutating_no_origin_still_authorized() {
    let d = DaemonHandle::spawn().await;
    let client = d.client(); // header channel only, no cookie, no Origin
    let resp = client
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "lm-10833-header-no-origin"}))
        .send()
        .await
        .unwrap();
    // CLI compatibility: header-only POST without Origin must succeed.
    assert_ne!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Stale header + fresh cookie (GET): the SPA may still carry a stale
/// localStorage value as `X-Clawket-Token` while the fresh bootstrap cookie
/// rides alongside. OR-semantics in the middleware mean the request is
/// authorized off the cookie regardless of the bad header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_header_with_fresh_cookie_authorizes_get() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/projects", d.base_url))
        .header("x-clawket-token", "stale-token-from-prior-restart")
        .header("cookie", format!("clawket_session={}", d.token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// Stale header + fresh cookie (POST + same Origin): same situation but on
/// a state-changing method with a same-origin Origin header — should also
/// pass (cookie auth + Origin guard satisfied).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_header_with_fresh_cookie_and_origin_authorizes_post() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let host = d.base_url.trim_start_matches("http://");
    let resp = bare
        .post(format!("{}/projects", d.base_url))
        .header("x-clawket-token", "stale-token-from-prior-restart")
        .header("cookie", format!("clawket_session={}", d.token))
        .header("origin", format!("http://{host}"))
        .json(&serde_json::json!({"name": "lm-10833-stale-header-fresh-cookie"}))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Stale header + fresh cookie (POST + foreign Origin): a stale header must
/// NOT bypass the origin guard. The mutating request from a foreign origin
/// is rejected with ORIGIN_MISMATCH even though the cookie is valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_header_with_fresh_cookie_foreign_origin_rejected() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let resp = bare
        .post(format!("{}/projects", d.base_url))
        .header("x-clawket-token", "stale-token-from-prior-restart")
        .header("cookie", format!("clawket_session={}", d.token))
        .header("origin", "http://evil.example")
        .json(&serde_json::json!({"name": "lm-10833-stale-header-foreign-origin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "ORIGIN_MISMATCH");
}

/// Wrong cookie value: a request whose cookie carries a stale/wrong token is
/// rejected with TOKEN_REQUIRED, just like the missing-token case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookie_with_wrong_token_rejected() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    let resp = bare
        .get(format!("{}/projects", d.base_url))
        .header("cookie", "clawket_session=not-the-real-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "TOKEN_REQUIRED");
}

/// SPA bootstrap channel must be reachable without auth. The SPA index is the
/// channel that *issues* the bootstrap cookie, so requiring the cookie on it
/// would close the bootstrap loop. Same applies to static assets carrying the
/// built JS/CSS — they're auth-exempt; only the data API stays gated.
///
/// This regression test pins the auth-exempt path list. If somebody removes
/// `/` (or `/assets/*`) from the exempt list in `tcp_auth_layer`, the unauthed
/// browser visit collapses back to a 401 white screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spa_bootstrap_paths_are_auth_exempt() {
    let d = DaemonHandle::spawn().await;
    let bare = reqwest::Client::new();
    // Each path must return a non-401 status without any token / cookie.
    for path in &[
        "/",
        "/web",
        "/favicon.svg",
        "/icons.svg",
        "/assets/anything.js",
    ] {
        let resp = bare
            .get(format!("{}{path}", d.base_url))
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "path {path} should be auth-exempt (got 401)"
        );
    }
}
