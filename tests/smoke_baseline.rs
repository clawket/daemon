// v2.2.1 baseline smoke tests (U7-T1..T6).
// Each test boots a fresh clawketd binary against a tempdir-isolated SQLite DB
// and exercises the route matrix that v2.2.1 Node implementations supported.
//
// These are behavioral parity tests — not full coverage. They answer:
// "does the Rust daemon accept the same inputs and return the same shapes as v2.2.1?"

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Daemon {
    child: Child,
    base: String,
    token: String,
    tmp: tempfile::TempDir,
}

impl Daemon {
    async fn spawn() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = env!("CARGO_BIN_EXE_clawketd");
        let cache_dir = tmp.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let child = Command::new(bin)
            .arg("--port")
            .arg("0")
            .arg("--db")
            .arg(tmp.path().join("test.sqlite"))
            .env("CLAWKET_DATA_DIR", tmp.path().join("data"))
            .env("CLAWKET_CACHE_DIR", &cache_dir)
            .env("CLAWKET_CONFIG_DIR", tmp.path().join("config"))
            .env("CLAWKET_STATE_DIR", tmp.path().join("state"))
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
        let port = port.expect("port file not written");
        let base = format!("http://127.0.0.1:{port}");

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

        let probe = reqwest::Client::new();
        for _ in 0..30 {
            if probe.get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Self {
            child,
            base,
            token,
            tmp,
        }
    }

    /// Build a reqwest client that injects the `X-Clawket-Token` header on
    /// every request (FIX-DAEMON-108).
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

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// U7-T1: plan/unit/task CRUD + approve (smoke matches v2.2.1 contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_crud_plan_unit_task_approve() {
    let d = Daemon::spawn().await;
    let c = d.client();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base))
        .json(&serde_json::json!({"name": "smoke-crud", "key": "SCR"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    // Happy path: approve plan → create unit → create+activate cycle → create task.
    let plan: serde_json::Value = c
        .post(format!("{}/plans", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    assert_eq!(plan["status"], "draft");

    // Approve plan unlocks unit+task creation.
    assert!(c
        .post(format!("{}/plans/{}/approve", d.base, plan_id))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // Unit is required before cycle (PDD A4: Cycle ⊂ Unit).
    let unit: serde_json::Value = c
        .post(format!("{}/units", d.base))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "u"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uid = unit["id"].as_str().unwrap().to_string();

    // A cycle is required for tasks to be startable — create + activate.
    let cycle: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "unit_id": uid, "title": "c"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cid = cycle["id"].as_str().unwrap().to_string();
    assert!(c
        .post(format!("{}/cycles/{}/activate", d.base, cid))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    let task: serde_json::Value = c
        .post(format!("{}/tasks", d.base))
        .json(&serde_json::json!({
            "unit_id": uid,
            "cycle_id": cid,
            "title": "t",
            "envelope": {
                "intent": "smoke",
                "prompt_template": "smoke prompt",
                "success_criteria": ["smoke ok"],
            },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(task["ticket_number"].as_str().unwrap().starts_with("SCR-"));

    // List tasks of the plan (by unit_id param).
    let list: Vec<serde_json::Value> = c
        .get(format!("{}/tasks?unit_id={}", d.base, uid))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], task["id"]);
}

// U7-T2: cycle lifecycle + automatic task cycle_id assignment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_cycle_lifecycle_and_task_mapping() {
    let d = Daemon::spawn().await;
    let c = d.client();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base))
        .json(&serde_json::json!({"name": "smk-cyc", "key": "SCY"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    // Plan → approve → unit (cycle requires unit_id per PDD A4).
    let plan: serde_json::Value = c
        .post(format!("{}/plans", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    c.post(format!("{}/plans/{}/approve", d.base, plan_id))
        .send()
        .await
        .unwrap();
    let unit: serde_json::Value = c
        .post(format!("{}/units", d.base))
        .json(&serde_json::json!({"plan_id": plan_id, "title": "u"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let uid = unit["id"].as_str().unwrap().to_string();

    // Create two cycles in the unit. Only one can be active at a time.
    let c1: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "unit_id": uid, "title": "one"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c1_id = c1["id"].as_str().unwrap().to_string();
    let _c2: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "unit_id": uid, "title": "two"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(c
        .post(format!("{}/cycles/{}/activate", d.base, c1_id))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // API-TASK-001: cycle_id is required at task creation; no auto-infer.
    let task: serde_json::Value = c
        .post(format!("{}/tasks", d.base))
        .json(&serde_json::json!({
            "unit_id": uid,
            "cycle_id": c1_id,
            "title": "t",
            "envelope": {
                "intent": "smoke",
                "prompt_template": "smoke prompt",
                "success_criteria": ["smoke ok"],
            },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(task["cycle_id"].as_str(), Some(c1_id.as_str()));

    // Resolve the task before completing the cycle (LM-241 residue guard).
    // EVIDENCE_REQUIRED: done transitions need evidence.
    let tid = task["id"].as_str().unwrap().to_string();
    c.patch(format!("{}/tasks/{}", d.base, tid))
        .json(&serde_json::json!({"status": "done", "evidence": "test:done"}))
        .send()
        .await
        .unwrap();

    // Complete the cycle → tasks should survive but no new cycle is active.
    assert!(c
        .post(format!("{}/cycles/{}/complete", d.base, c1_id))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

// U7-T3: knowledge + embeddings path. Search may fall back to keyword if embeddings
// are unavailable in CI, so we test the keyword path which is always present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_knowledge_keyword_search() {
    let d = Daemon::spawn().await;
    let c = d.client();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base))
        .json(&serde_json::json!({"name": "smk-art", "key": "SAR"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = project["id"].as_str().unwrap().to_string();
    let plan: serde_json::Value = c
        .post(format!("{}/plans", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "p"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_id = plan["id"].as_str().unwrap().to_string();
    c.post(format!("{}/plans/{}/approve", d.base, plan_id))
        .send()
        .await
        .unwrap();

    let _a: serde_json::Value = c
        .post(format!("{}/knowledge", d.base))
        .json(&serde_json::json!({
            "plan_id": plan_id,
            "title": "RAG chunking strategy",
            "content": "We will chunk by 512 tokens overlapping 64. Decision: confirmed.",
            "type": "decision",
            "content_format": "md"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Keyword search should match on content.
    // Response shape: { hits: [...], total_returned, limit, truncated } per
    // SearchResponse in routes/knowledge.rs.
    let resp: serde_json::Value = c
        .get(format!(
            "{}/knowledge/search?q=chunking&mode=keyword",
            d.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // KnowledgeHit serializes its fields flat (`#[serde(flatten)]`),
    // so `title` is on the hit itself, not under a nested key.
    let hits = resp["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|h| h["title"] == "RAG chunking strategy"),
        "keyword search should find chunking entry; got {resp:?}"
    );
}

// U7-T4: dashboard injection path — what the SessionStart hook calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_dashboard_picks_up_project_by_cwd() {
    let d = Daemon::spawn().await;
    let c = d.client();

    let dir = d.tmp.path().join("cwd-project");
    std::fs::create_dir_all(&dir).unwrap();

    let project: serde_json::Value = c
        .post(format!("{}/projects", d.base))
        .json(&serde_json::json!({
            "name": "hook-smoke",
            "key": "HK",
            "cwd": dir.to_string_lossy()
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(project["key"], "HK");
    let cwds = project["cwds"].as_array().cloned().unwrap_or_default();
    assert!(
        !cwds.is_empty(),
        "project should have at least one cwd registered, got {project:?}"
    );

    // The SessionStart hook calls /dashboard?cwd=... Use reqwest query to get URL-encoding right.
    let dash: serde_json::Value = c
        .get(format!("{}/dashboard", d.base))
        .query(&[("cwd", dir.to_string_lossy().as_ref())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // /dashboard returns project as the project id string (v2.2.1 parity).
    assert_eq!(
        dash["project"], "PROJ-hook-smoke",
        "dashboard cwd lookup failed; full response: {dash}"
    );
    let ctx = dash["context"].as_str().unwrap_or("");
    assert!(
        ctx.contains("hook-smoke"),
        "dashboard context should mention project name, got: {ctx}"
    );
}

// MCP tools/list surface is covered by the cli crate's tests/mcp_compat.rs
// (spawns `clawket mcp` over stdio and asserts the v3 read-only tool set).
// The daemon does not expose MCP, so no MCP smoke test lives here.

// Legacy lattice→clawket migration was removed. Clawket now treats every install
// as a fresh start and only emits a stderr warning when legacy data is detected.
// See paths::warn_legacy_data.
