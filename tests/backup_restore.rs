// End-to-end HTTP tests for POST /backup and POST /restore against the real
// clawketd binary. The CLI has called both paths since v3.0.0; these assert
// they are actually mounted and that the contract in `repo::backup` holds over
// the wire (round trip preserves data, dry_run mutates nothing, corrupt and
// too-new archives are refused, --merge is refused rather than downgraded).

use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct DaemonHandle {
    child: Child,
    base_url: String,
    token: String,
    tmpdir: tempfile::TempDir,
    db_path: std::path::PathBuf,
}

impl DaemonHandle {
    async fn spawn() -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let db_path = tmpdir.path().join("data").join("test.sqlite");
        let bin = env!("CARGO_BIN_EXE_clawketd");
        let cache_dir = tmpdir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(tmpdir.path().join("data")).unwrap();

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
            tmpdir,
            db_path,
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

    /// Create a project so the database carries data a round trip can lose.
    async fn seed_project(&self, name: &str) -> String {
        let client = self.client();
        let resp: serde_json::Value = client
            .post(format!("{}/projects", self.base_url))
            .json(&serde_json::json!({
                "name": name,
                "cwd": self.tmpdir.path().join(name).to_string_lossy(),
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp["id"]
            .as_str()
            .unwrap_or_else(|| panic!("project create failed: {resp}"))
            .to_string()
    }

    async fn project_names(&self) -> Vec<String> {
        let client = self.client();
        let resp: serde_json::Value = client
            .get(format!("{}/projects", self.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let arr = resp.as_array().cloned().unwrap_or_default();
        arr.iter()
            .filter_map(|p| p["name"].as_str().map(str::to_string))
            .collect()
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_route_is_mounted_and_writes_an_archive() {
    let d = DaemonHandle::spawn().await;
    let out = d.tmpdir.path().join("snap.clawketbak.gz");

    let resp = d
        .client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": out.to_string_lossy(), "project_id": null }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "POST /backup must be mounted");
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["output"], out.to_string_lossy().as_ref());
    assert!(body["bytes"].as_u64().unwrap() > 0);
    assert!(body["schema_version"].as_i64().unwrap() > 0);
    // The archive is always the whole store — the response says so explicitly.
    assert_eq!(body["scope"], "full-database");
    assert!(out.exists(), "archive file should exist on disk");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_trip_restores_data_lost_after_the_backup() {
    let d = DaemonHandle::spawn().await;
    let kept = d.seed_project("kept-project").await;
    assert!(kept.starts_with("PROJ-"));

    let out = d.tmpdir.path().join("rt.clawketbak.gz");
    let resp = d
        .client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": out.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Add data that exists only after the snapshot was taken.
    d.seed_project("added-later").await;
    let before = d.project_names().await;
    assert!(before.contains(&"added-later".to_string()));

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({
            "input": out.to_string_lossy(), "merge": false, "dry_run": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["restored"], true);
    assert_eq!(body["dry_run"], false);
    assert!(body["previous_db_backup"].is_string());

    // Inspect the restored file directly rather than through the daemon: the
    // running process still holds pool connections against the replaced file,
    // which is exactly why the response tells the caller to restart.
    let conn = rusqlite::Connection::open(&d.db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM projects ORDER BY name")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        names.contains(&"kept-project".to_string()),
        "restored db lost pre-backup data: {names:?}"
    );
    assert!(
        !names.contains(&"added-later".to_string()),
        "restored db still holds post-backup data: {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_reports_without_changing_anything() {
    let d = DaemonHandle::spawn().await;
    d.seed_project("only-project").await;

    let out = d.tmpdir.path().join("dry.clawketbak.gz");
    d.client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": out.to_string_lossy() }))
        .send()
        .await
        .unwrap();

    d.seed_project("post-backup").await;
    let db_mtime_before = std::fs::metadata(&d.db_path).unwrap().modified().unwrap();

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({
            "input": out.to_string_lossy(), "merge": false, "dry_run": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["restored"], false);
    assert!(body["previous_db_backup"].is_null());
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("Nothing was changed"));

    // Live data survives, no pre-restore copy was taken, db file untouched.
    let after = d.project_names().await;
    assert!(after.contains(&"post-backup".to_string()));
    let pre_restore = d.db_path.with_extension("sqlite.pre-restore");
    assert!(!pre_restore.exists(), "dry run must not copy the live db");
    assert_eq!(
        db_mtime_before,
        std::fs::metadata(&d.db_path).unwrap().modified().unwrap(),
        "dry run must not touch the live db file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_is_refused_with_code_and_reason() {
    let d = DaemonHandle::spawn().await;
    let out = d.tmpdir.path().join("m.clawketbak.gz");
    d.client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": out.to_string_lossy() }))
        .send()
        .await
        .unwrap();

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({
            "input": out.to_string_lossy(), "merge": true, "dry_run": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "--merge must fail loudly, not silently replace"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "MERGE_NOT_SUPPORTED");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("remapping generated ids"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_archive_is_refused_and_live_db_survives() {
    let d = DaemonHandle::spawn().await;
    d.seed_project("survivor").await;

    let junk = d.tmpdir.path().join("junk.gz");
    std::fs::write(&junk, b"not a gzip stream at all").unwrap();

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({
            "input": junk.to_string_lossy(), "merge": false, "dry_run": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BACKUP_ARCHIVE_INVALID");

    // The database the daemon is serving is untouched.
    assert!(d.project_names().await.contains(&"survivor".to_string()));
    assert!(!d.db_path.with_extension("sqlite.pre-restore").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_archive_is_404_and_relative_path_is_400() {
    let d = DaemonHandle::spawn().await;

    let missing = d.tmpdir.path().join("nope.clawketbak.gz");
    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": missing.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BACKUP_ARCHIVE_NOT_FOUND");

    // A relative path would resolve against the daemon's cwd, not the caller's.
    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": "some/relative.gz" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "INVALID_BACKUP_PATH");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_from_a_newer_schema_is_refused() {
    let d = DaemonHandle::spawn().await;
    d.seed_project("survivor").await;

    // Take a real archive, then rewrite its header to claim a schema version
    // far beyond anything this binary supports.
    let out = d.tmpdir.path().join("future.clawketbak.gz");
    d.client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": out.to_string_lossy() }))
        .send()
        .await
        .unwrap();

    let forged = forge_header_version(&out, 9999);
    let forged_path = d.tmpdir.path().join("forged.clawketbak.gz");
    std::fs::write(&forged_path, forged).unwrap();

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": forged_path.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "SCHEMA_DOWNGRADE_REFUSED");
    assert!(d.project_names().await.contains(&"survivor".to_string()));
}

/// Re-encode an archive with `schema_version` overwritten in its JSON header.
/// Mirrors the on-disk layout documented in `repo::backup`:
/// `gzip(MAGIC | u32le header_len | header_json | sqlite_bytes)`.
fn forge_header_version(path: &std::path::Path, version: i64) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{Read, Write};

    let mut raw = Vec::new();
    GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_end(&mut raw)
        .unwrap();

    let magic = &raw[0..8];
    let header_len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let mut header: serde_json::Value = serde_json::from_slice(&raw[12..12 + header_len]).unwrap();
    header["schema_version"] = serde_json::json!(version);
    let new_header = serde_json::to_vec(&header).unwrap();
    let payload = &raw[12 + header_len..];

    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(magic).unwrap();
    enc.write_all(&(new_header.len() as u32).to_le_bytes())
        .unwrap();
    enc.write_all(&new_header).unwrap();
    enc.write_all(payload).unwrap();
    enc.finish().unwrap()
}
