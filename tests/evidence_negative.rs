// C5 — EVIDENCE_REQUIRED HTTP-level normalization tests.
//
// The repo-level guard (`src/repo/tasks.rs:654-661`) trims evidence and checks
// `is_empty()`. The route layer's `value_to_opt_string`
// (`src/routes/util.rs:15-22`) is responsible for collapsing `""`, `"   "`,
// `null`, and non-string types to `None` before the repo sees them. These
// integration tests exercise the full path end-to-end so that a future change
// in either layer cannot silently let an empty evidence slip into a
// `status=done` transition.

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
            .env("CLAWKET_WEB_DIR", tmpdir.path().join("no-web"))
            .env("CLAWKETD_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn clawketd");

        let port_file = cache_dir.join("clawketd.port");
        let mut port: Option<u16> = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(s) = std::fs::read_to_string(&port_file) {
                if let Ok(p) = s.trim().parse::<u16>() {
                    port = Some(p);
                    break;
                }
            }
        }
        let port = port.expect("daemon port file not written");
        let base_url = format!("http://127.0.0.1:{port}");

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

/// Bootstrap a project → approved plan → unit → active cycle → todo task,
/// returning the task id ready for status transitions.
async fn bootstrap_task(d: &DaemonHandle) -> String {
    let c = d.client();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "ev-neg", "key": "EVN"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    let plan: serde_json::Value = c
        .post(format!("{}/plans", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    c.post(format!("{}/plans/{}/approve", d.base_url, plan_id))
        .send()
        .await
        .unwrap();

    let unit: serde_json::Value = c
        .post(format!("{}/units", d.base_url))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "u"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uid = unit["id"].as_str().unwrap().to_string();

    let cycle: serde_json::Value = c
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({"project_id": pid, "unit_id": uid, "title": "c"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cid = cycle["id"].as_str().unwrap().to_string();
    c.post(format!("{}/cycles/{}/activate", d.base_url, cid))
        .send()
        .await
        .unwrap();

    let task: serde_json::Value = c
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": uid,
            "cycle_id": cid,
            "title": "t",
            "envelope": {
                "intent": "evidence test",
                "prompt_template": "evidence test prompt",
                "success_criteria": ["evidence ok"],
            },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    task["id"].as_str().unwrap().to_string()
}

async fn assert_evidence_required(d: &DaemonHandle, tid: &str, body: serde_json::Value) {
    let c = d.client();
    let resp = c
        .patch(format!("{}/tasks/{}", d.base_url, tid))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        status.as_u16(),
        400,
        "expected HTTP 400, got {} (body: {})",
        status,
        json
    );
    assert_eq!(
        json["code"].as_str(),
        Some("EVIDENCE_REQUIRED"),
        "expected code=EVIDENCE_REQUIRED, got body: {}",
        json
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_with_empty_string_evidence_is_rejected() {
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    assert_evidence_required(
        &d,
        &tid,
        serde_json::json!({"status": "done", "evidence": ""}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_with_whitespace_only_evidence_is_rejected() {
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    assert_evidence_required(
        &d,
        &tid,
        serde_json::json!({"status": "done", "evidence": "   \t\n  "}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_with_null_evidence_is_rejected() {
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    assert_evidence_required(
        &d,
        &tid,
        serde_json::json!({"status": "done", "evidence": null}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_without_evidence_field_is_rejected_when_task_has_none() {
    // Missing field falls back to the task's existing evidence
    // (`tasks.rs:651-653`). On a fresh task with no prior evidence the guard
    // must still fire.
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    assert_evidence_required(&d, &tid, serde_json::json!({"status": "done"})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_with_non_string_evidence_is_rejected() {
    // `value_to_opt_string` collapses non-string JSON types to None
    // (`util.rs:15-22`). A numeric evidence value must therefore fall through
    // to EVIDENCE_REQUIRED, not silently coerce.
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    assert_evidence_required(
        &d,
        &tid,
        serde_json::json!({"status": "done", "evidence": 42}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_succeeds_with_non_empty_evidence() {
    // Positive control: the guard must accept a real evidence string so the
    // negative tests above are not just rejecting every PATCH.
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    let c = d.client();
    let resp = c
        .patch(format!("{}/tasks/{}", d.base_url, tid))
        .json(&serde_json::json!({"status": "done", "evidence": "src/foo.rs:42"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "expected 2xx for valid evidence, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "done");
    assert_eq!(body["evidence"], "src/foo.rs:42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_succeeds_when_prior_evidence_persists_through_missing_field() {
    // Once evidence is set on a task, a later PATCH that omits the evidence
    // field but transitions status (e.g., undone → done after a follow-up)
    // must reuse the persisted value (`tasks.rs:651-653`). The exercise here
    // is: set evidence while still todo → transition done without resending.
    let d = DaemonHandle::spawn().await;
    let tid = bootstrap_task(&d).await;
    let c = d.client();
    c.patch(format!("{}/tasks/{}", d.base_url, tid))
        .json(&serde_json::json!({"evidence": "logs/run-001.txt"}))
        .send()
        .await
        .unwrap();
    let resp = c
        .patch(format!("{}/tasks/{}", d.base_url, tid))
        .json(&serde_json::json!({"status": "done"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "expected 2xx (prior evidence carries over), got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "done");
    assert_eq!(body["evidence"], "logs/run-001.txt");
}
