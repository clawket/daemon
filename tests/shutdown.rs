// Shutdown timing tests for clawketd.
//
// CK-536 (escalated from CK-533/534): axum's `with_graceful_shutdown` only
// stops accepting new connections; it waits for existing keep-alive
// connections to close naturally. A single long-lived client (e.g. an open
// web dashboard tab) blocks shutdown indefinitely, so the daemon now
// force-exits after a grace window. Override the window with
// CLAWKETD_SHUTDOWN_GRACE_MS for fast tests.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    port: u16,
    _tmpdir: tempfile::TempDir,
}

impl Daemon {
    async fn spawn(grace_ms: u64) -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let db_path = tmpdir.path().join("test.sqlite");
        let cache_dir = tmpdir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let bin = env!("CARGO_BIN_EXE_clawketd");
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
            .env("CLAWKETD_SHUTDOWN_GRACE_MS", grace_ms.to_string())
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

        let client = reqwest::Client::new();
        for _ in 0..30 {
            if client
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Self {
            child,
            port,
            _tmpdir: tmpdir,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Belt-and-braces cleanup; tests should already have observed exit.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn send_sigterm(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid as i32, libc::SIGTERM);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_exits_within_grace_when_keep_alive_client_holds_connection() {
    // 500ms grace + ~500ms slack for process exit.
    let mut daemon = Daemon::spawn(500).await;
    let pid = daemon.child.id();

    // Open a keep-alive client that holds a TCP connection. Don't drop it
    // before SIGTERM; this reproduces the Chrome-tab-holding-the-dashboard
    // scenario that previously hung graceful_shutdown indefinitely.
    let pool_idle = Duration::from_secs(60);
    let client = reqwest::Client::builder()
        .pool_idle_timeout(pool_idle)
        .pool_max_idle_per_host(8)
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://127.0.0.1:{}/health", daemon.port))
        .send()
        .await
        .expect("first health request");
    assert!(resp.status().is_success());
    // Hold `client` alive across the SIGTERM so the keep-alive connection
    // remains in the daemon's accept set.

    let start = Instant::now();
    send_sigterm(pid);

    // Poll for child exit. Should be ~500ms (grace) + small overhead.
    let mut exited_in: Option<Duration> = None;
    for _ in 0..30 {
        match daemon.child.try_wait() {
            Ok(Some(_status)) => {
                exited_in = Some(start.elapsed());
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(_) => break,
        }
    }
    drop(client);

    let elapsed = exited_in.expect("daemon must exit after grace period");
    assert!(
        elapsed < Duration::from_secs(2),
        "expected exit within 2s, took {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_exits_immediately_when_no_clients() {
    let mut daemon = Daemon::spawn(3000).await;
    let pid = daemon.child.id();

    let start = Instant::now();
    send_sigterm(pid);

    let mut exited_in: Option<Duration> = None;
    for _ in 0..50 {
        match daemon.child.try_wait() {
            Ok(Some(_)) => {
                exited_in = Some(start.elapsed());
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => break,
        }
    }
    let elapsed = exited_in.expect("daemon must exit cleanly with no clients");
    // Without clients, graceful_shutdown completes instantly — well below the
    // 3s grace cap. If this regresses to >2s we've broken the fast path.
    assert!(
        elapsed < Duration::from_secs(2),
        "expected fast exit (<2s), took {elapsed:?}"
    );
}
