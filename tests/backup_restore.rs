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
    assert_eq!(body["code"], "ARCHIVE_SCHEMA_TOO_NEW");
    assert!(d.project_names().await.contains(&"survivor".to_string()));
}

/// Re-encode an archive with `schema_version` overwritten in its JSON header.
/// Mirrors the on-disk layout documented in `repo::backup`:
/// `gzip(MAGIC | u32le header_len | header_json | sqlite_bytes)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_refuses_to_write_over_the_live_database() {
    // `File::create` truncates unconditionally, so before the guard an
    // `--output` naming the database replaced the store with gzip magic and
    // still returned 200 — the user was told the backup succeeded at the moment
    // their data was destroyed. Restore's pre-swap copy does not help here:
    // there is no earlier state to fall back to.
    let d = DaemonHandle::spawn().await;
    let before = std::fs::metadata(&d.db_path).unwrap().len();

    for target in [
        d.db_path.clone(),
        d.db_path.with_extension("sqlite-wal"),
        d.db_path.with_extension("sqlite-shm"),
    ] {
        let resp = d
            .client()
            .post(format!("{}/backup", d.base_url))
            .json(&serde_json::json!({ "output": target.to_string_lossy() }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "backup onto {} must be refused",
            target.display()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["code"], "INVALID_BACKUP_PATH");
    }

    // Spelling variants of the same file must be refused too. The guard used to
    // compare strings, and only `output` was normalised — `db_path` arrives
    // verbatim from `Paths::resolve()`, so any difference in spelling slipped
    // through and truncated the store with a 200. A `.` component is the
    // cheapest reproduction; on macOS `/tmp` vs `/private/tmp` does it with no
    // unusual input at all.
    // A symlinked parent directory is the vector: `sanitize_path` collapses `.`
    // and `..` lexically, so those still matched a string compare — a symlink
    // does not, because resolving it needs the filesystem. `/tmp` being a link
    // to `/private/tmp` on macOS makes this the DEFAULT spelling there, not an
    // exotic one.
    let parent = d.db_path.parent().unwrap();
    let name = d.db_path.file_name().unwrap();
    let link = d.tmpdir.path().join("data-link");
    std::os::unix::fs::symlink(parent, &link).unwrap();
    for alias in [link.join(name)] {
        let resp = d
            .client()
            .post(format!("{}/backup", d.base_url))
            .json(&serde_json::json!({ "output": alias.to_string_lossy() }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "an alias of the live database must be refused: {}",
            alias.display()
        );
    }

    // The store is untouched and still a database.
    assert_eq!(std::fs::metadata(&d.db_path).unwrap().len(), before);
    let conn = rusqlite::Connection::open(&d.db_path).unwrap();
    conn.query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))
        .expect("database is still readable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_after_a_restore_are_refused_not_silently_dropped() {
    // The pool holds the replaced inode after the swap, so a write commits into
    // a file nothing will reopen: it returns success and is gone once the
    // connection closes. Refusing is the only honest answer until a restart.
    let d = DaemonHandle::spawn().await;
    d.seed_project("before-backup").await;

    let archive = d.tmpdir.path().join("pending.clawketbak.gz");
    let resp = d
        .client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": archive.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // A dry run changes nothing, so it must NOT latch the flag.
    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": archive.to_string_lossy(), "dry_run": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let still_writable = d
        .client()
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({ "name": "after-dry-run" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        still_writable.status(),
        200,
        "a dry run must leave the daemon writable"
    );

    // The real thing latches it.
    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": archive.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["restored"],
        true
    );

    let refused = d
        .client()
        .post(format!("{}/projects", d.base_url))
        .json(&serde_json::json!({ "name": "would-be-lost" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        503,
        "a write after restore must be refused, not accepted and discarded"
    );
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(body["code"], "RESTART_REQUIRED");

    // Reads still work — the daemon is degraded, not dead.
    let read = d
        .client()
        .get(format!("{}/projects", d.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_refuses_a_destination_inside_the_plugin_tree() {
    // LM-8: `/plugin install` deletes `~/.claude/plugins/` wholesale, so a
    // backup written there is destroyed by the next reinstall — exactly when the
    // user reaches for it. Arbitrary destinations are otherwise allowed on
    // purpose (offsite backup needs them); this is the one carve-out.
    let d = DaemonHandle::spawn().await;
    let target = d
        .tmpdir
        .path()
        .join(".claude")
        .join("plugins")
        .join("clawket-x")
        .join("backup.gz");

    let resp = d
        .client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": target.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "INVALID_BACKUP_PATH");
    assert!(!target.exists(), "nothing should have been written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_that_is_not_a_usable_database_is_refused() {
    // The gzip stream, magic and header are all intact here — only the SQLite
    // payload is damaged. That is the case `PRAGMA integrity_check` exists for,
    // and the reason the existing corrupt-archive test does not cover it: that
    // one breaks the gzip framing, so decoding fails long before a database is
    // ever opened.
    let d = DaemonHandle::spawn().await;
    d.seed_project("keep-me").await;

    let good = d.tmpdir.path().join("good.clawketbak.gz");
    let resp = d
        .client()
        .post(format!("{}/backup", d.base_url))
        .json(&serde_json::json!({ "output": good.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let damaged = d.tmpdir.path().join("damaged.clawketbak.gz");
    std::fs::write(&damaged, forge_damaged_payload(&good)).unwrap();

    let resp = d
        .client()
        .post(format!("{}/restore", d.base_url))
        .json(&serde_json::json!({ "input": damaged.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "a broken payload must not be installed");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BACKUP_ARCHIVE_INVALID");

    // The live store is untouched and still serving.
    let read = d
        .client()
        .get(format!("{}/projects", d.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 200);
}

/// Re-wrap an archive with its SQLite payload corrupted past page 0, leaving the
/// gzip stream, magic and JSON header valid. The header is what `decode_archive`
/// checks; the payload is what `integrity_check` checks.
fn forge_damaged_payload(path: &std::path::Path) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{Read, Write};

    let mut raw = Vec::new();
    GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_end(&mut raw)
        .unwrap();

    let header_len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
    let split = 12 + header_len;
    let (head, payload) = raw.split_at(split);
    let mut payload = payload.to_vec();
    // Corrupt ONE interior page, leaving the file openable and the page count in
    // the header consistent. A wholesale scribble makes SQLite refuse to open
    // the file at all, which is caught earlier and would leave the
    // integrity_check branch itself untested. The database page size defaults to
    // 4096, so page 3 starts at 8192 — well past the schema SQLite reads to open.
    let start = 8192.min(payload.len().saturating_sub(1));
    let end = (start + 512).min(payload.len());
    for b in payload[start..end].iter_mut() {
        *b = 0xAB;
    }

    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(head).unwrap();
    enc.write_all(&payload).unwrap();
    enc.finish().unwrap()
}

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
