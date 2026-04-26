// End-to-end HTTP parity tests against the real clawketd binary.
// Each test starts the binary on a random port against a fresh SQLite DB,
// exercises the route, and tears down.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct DaemonHandle {
    child: Child,
    base_url: String,
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

        // Wait for health.
        let client = reqwest::Client::new();
        for _ in 0..30 {
            if client.get(format!("{base_url}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Self {
            child,
            base_url,
            _tmpdir: tmpdir,
        }
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
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_project_plan_task_flow() {
    let d = DaemonHandle::spawn().await;
    let client = reqwest::Client::new();

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

    // Create cycle & activate (tasks need active cycle)
    let cycle: serde_json::Value = client
        .post(format!("{}/cycles", d.base_url))
        .json(&serde_json::json!({"project_id": project_id, "title": "itest cycle"}))
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

    // Create unit
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

    // Create task
    let task: serde_json::Value = client
        .post(format!("{}/tasks", d.base_url))
        .json(&serde_json::json!({"unit_id": unit_id, "title": "Hello task"}))
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
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_returns_no_project_message() {
    let d = DaemonHandle::spawn().await;
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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
    let client = reqwest::Client::new();
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
async fn static_root_returns_503_when_no_web_dir() {
    let d = DaemonHandle::spawn().await;
    let client = reqwest::Client::new();
    let resp = client.get(format!("{}/", d.base_url)).send().await.unwrap();
    // No web/dist built during test runs → dashboardNotBuilt fallback
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}
