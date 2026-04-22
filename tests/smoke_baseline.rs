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

        let c = reqwest::Client::new();
        for _ in 0..30 {
            if c.get(format!("{base}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Self { child, base, tmp }
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
    let c = reqwest::Client::new();

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

    // Plan starts in draft. Creating tasks under draft plan should fail.
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

    // A cycle is required for tasks to be startable — create + activate.
    let cycle: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "c"}))
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

    // Approve plan unlocks unit+task creation.
    assert!(c
        .post(format!("{}/plans/{}/approve", d.base, plan_id))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

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

    let task: serde_json::Value = c
        .post(format!("{}/tasks", d.base))
        .json(&serde_json::json!({"unit_id": uid, "title": "t"}))
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
    let c = reqwest::Client::new();

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

    // Create two cycles; only one can be active at a time.
    let c1: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "one"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let c1_id = c1["id"].as_str().unwrap().to_string();
    let _c2: serde_json::Value = c
        .post(format!("{}/cycles", d.base))
        .json(&serde_json::json!({"project_id": pid, "title": "two"}))
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

    // Create plan→approve→unit→task; new task should get cycle_id=c1.
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

    let task: serde_json::Value = c
        .post(format!("{}/tasks", d.base))
        .json(&serde_json::json!({"unit_id": uid, "title": "t"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(task["cycle_id"].as_str(), Some(c1_id.as_str()));

    // Complete the cycle → tasks should survive but no new cycle is active.
    assert!(c
        .post(format!("{}/cycles/{}/complete", d.base, c1_id))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

// U7-T3: artifacts + embeddings path. Search may fall back to keyword if embeddings
// are unavailable in CI, so we test the keyword path which is always present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_artifacts_keyword_search() {
    let d = Daemon::spawn().await;
    let c = reqwest::Client::new();

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
        .post(format!("{}/artifacts", d.base))
        .json(&serde_json::json!({
            "plan_id": plan_id,
            "title": "RAG chunking strategy",
            "content": "We will chunk by 512 tokens overlapping 64. Decision: confirmed.",
            "type": "decision",
            "content_format": "md",
            "scope": "rag"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Keyword search should match on content.
    let hits: Vec<serde_json::Value> = c
        .get(format!(
            "{}/artifacts/search?q=chunking&mode=keyword&scope=rag",
            d.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h["title"] == "RAG chunking strategy"),
        "keyword search should find chunking artifact; got {hits:?}"
    );
}

// U7-T4: dashboard injection path — what the SessionStart hook calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_dashboard_picks_up_project_by_cwd() {
    let d = Daemon::spawn().await;
    let c = reqwest::Client::new();

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

// U7-T5: MCP RAG tools surface (5 tools listed).
// Starts the MCP Rust binary from the sibling crate via a hand-rolled Command.
// Skipped when the MCP binary isn't available in the release workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_mcp_rag_tools_list() {
    // The MCP binary lives in ../mcp/rust — built separately. Best effort here.
    let candidates = [
        "../mcp/rust/target/release/clawket-mcp-rs",
        "../mcp/rust/target/debug/clawket-mcp-rs",
    ];
    let bin = candidates.iter().find(|p| std::path::Path::new(p).is_file());
    let bin = match bin {
        Some(p) => *p,
        None => {
            eprintln!("[smoke] skip mcp: no built clawket-mcp-rs binary");
            return;
        }
    };

    use std::io::Write;
    let mut child = Command::new(bin)
        .env("CLAWKET_DAEMON_URL", "http://127.0.0.1:0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
        )
        .unwrap();
    }
    let out = child.wait_with_output().expect("wait mcp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    let tools_resp: serde_json::Value = serde_json::from_str(lines[1]).expect("tools/list");
    let names: Vec<&str> = tools_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "clawket_search_artifacts",
        "clawket_search_tasks",
        "clawket_find_similar_tasks",
        "clawket_get_task_context",
        "clawket_get_recent_decisions",
    ] {
        assert!(
            names.contains(&expected),
            "MCP should expose {expected}; got {names:?}"
        );
    }
}

// U7-T6: legacy lattice→clawket DB migration. Spawns daemon with a fake legacy DB
// under $XDG_DATA_HOME/lattice/db.sqlite and verifies it's copied into the clawket path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smoke_legacy_lattice_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg_data = tmp.path().join("xdg-data");
    let legacy = xdg_data.join("lattice");
    std::fs::create_dir_all(&legacy).unwrap();
    let legacy_db = legacy.join("db.sqlite");

    // Create a sentinel file that contains the SQLite magic so the daemon accepts it
    // as a fresh DB on open. Using rusqlite would be cleaner but we don't depend on it
    // from tests; writing an empty file is enough to trigger the copy path, and the
    // daemon will then initialize the empty file as a new DB on first open.
    std::fs::write(&legacy_db, b"").unwrap();

    let bin = env!("CARGO_BIN_EXE_clawketd");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    // Point CLAWKET_DATA_DIR explicitly so the migration knows the target,
    // but keep XDG_DATA_HOME set so migrate_legacy_data sees the legacy path.
    let data_dir = tmp.path().join("clawket-data");
    let child = Command::new(bin)
        .arg("--port")
        .arg("0")
        .env("CLAWKET_DATA_DIR", &data_dir)
        .env("CLAWKET_CACHE_DIR", &cache_dir)
        .env("CLAWKET_CONFIG_DIR", tmp.path().join("config"))
        .env("CLAWKET_STATE_DIR", tmp.path().join("state"))
        .env("XDG_DATA_HOME", &xdg_data)
        .env("CLAWKETD_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn clawketd");
    let mut child = child;

    // Wait for port file to confirm boot.
    let port_file = cache_dir.join("clawketd.port");
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if port_file.is_file() {
            ready = true;
            break;
        }
    }
    assert!(ready, "daemon did not boot");

    // Legacy db should have been renamed with .migrated-to-clawket suffix,
    // and clawket db should now exist.
    assert!(
        legacy.join("db.sqlite.migrated-to-clawket").is_file(),
        "legacy db should be renamed with .migrated-to-clawket suffix"
    );
    assert!(
        data_dir.join("db.sqlite").is_file(),
        "clawket db should exist after migration"
    );

    let _ = child.kill();
    let _ = child.wait();
}
