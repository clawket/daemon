// LM-11058 — task-create atomicity regression.
//
// The entropy/secret guard (`src/secrets/redact.rs::reject_high_entropy_in_value`)
// is a pure validation with no DB side-effect. It used to run *after*
// `repo::tasks::create` had already committed its own transaction, so a
// rejected envelope returned HTTP 400 but left an orphan task row behind
// (body="", active_envelope_id=NULL) that accumulated on every retry.
//
// The fix moves that guard to *before* the INSERT, co-located with the
// structural required-field gate (`src/routes/tasks.rs:325-333`). These
// tests pin the invariant end-to-end: a rejected envelope must create no
// task, and a clean envelope must still create exactly one.

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

/// Bootstrap a project → approved plan → unit → active cycle and return the
/// `(unit_id, cycle_id)`. Stops *before* creating any task so callers can
/// count tasks in the cycle with a clean baseline of zero.
async fn bootstrap_cycle(d: &DaemonHandle) -> (String, String) {
    let c = d.client();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({"name": "orphan", "key": "ORP"}))
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

    (uid, cid)
}

/// Count tasks currently attached to a cycle via the public GET /tasks
/// filter — the same surface the dashboard and CLI observe.
async fn count_tasks_in_cycle(d: &DaemonHandle, cid: &str) -> usize {
    let c = d.client();
    let tasks: serde_json::Value = c
        .get(format!("{}/tasks?cycle_id={}", d.base_url, cid))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    tasks.as_array().expect("GET /tasks returns an array").len()
}

/// A structurally-valid envelope (all three required fields present) that
/// nonetheless carries a high-entropy secret in an extra leaf. This passes
/// the required-field gate but must be rejected by the entropy guard — so it
/// exercises exactly the validation that used to run *after* the INSERT.
fn envelope_with_secret() -> serde_json::Value {
    serde_json::json!({
        "intent": "Wire the billing integration",
        "prompt_template": "Add the Stripe webhook handler and verify signatures",
        "success_criteria": ["webhook signature verified"],
        // 41 distinct chars → ~5.36 bits/char, length > 20 → trips the guard.
        "api_key": "abcdefghijklmnopqrstuvwxyz0123456789ABCDE",
    })
}

fn clean_envelope() -> serde_json::Value {
    serde_json::json!({
        "intent": "Wire the billing integration",
        "prompt_template": "Add the Stripe webhook handler and verify signatures",
        "success_criteria": ["webhook signature verified"],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entropy_rejected_envelope_leaves_no_orphan_task() {
    let d = DaemonHandle::spawn().await;
    let (uid, cid) = bootstrap_cycle(&d).await;
    let c = d.client();

    assert_eq!(
        count_tasks_in_cycle(&d, &cid).await,
        0,
        "baseline: cycle must start with zero tasks"
    );

    // Retry the rejected create several times: pre-fix, each attempt left a
    // fresh orphan, so the count would climb to N. The invariant is that it
    // stays at zero no matter how many times the caller retries.
    for attempt in 0..3 {
        let resp = c
            .post(format!("{}/tasks", d.base_url))
            .json(&serde_json::json!({
                "unit_id": uid,
                "cycle_id": cid,
                "title": "secret-bearing task",
                "envelope": envelope_with_secret(),
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            status.as_u16(),
            400,
            "attempt {attempt}: expected HTTP 400, got {status} (body: {json})"
        );
        assert!(
            json["error"]
                .as_str()
                .map(|e| e.contains("looks like a secret"))
                .unwrap_or(false),
            "attempt {attempt}: expected secret-guard message, got body: {json}"
        );

        assert_eq!(
            count_tasks_in_cycle(&d, &cid).await,
            0,
            "attempt {attempt}: rejected create must not leave an orphan task"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_envelope_creates_exactly_one_task() {
    // Positive control: the entropy gate must not block legitimate envelopes,
    // so the orphan assertion above is not just rejecting every create.
    let d = DaemonHandle::spawn().await;
    let (uid, cid) = bootstrap_cycle(&d).await;
    let c = d.client();

    let resp = c
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({
            "unit_id": uid,
            "cycle_id": cid,
            "title": "clean task",
            "envelope": clean_envelope(),
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "expected 2xx for a clean envelope, got {}",
        resp.status()
    );

    assert_eq!(
        count_tasks_in_cycle(&d, &cid).await,
        1,
        "a clean envelope must create exactly one task"
    );
}
