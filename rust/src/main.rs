mod config;
mod db;
mod embeddings;
mod id;
mod import_plan;
mod models;
mod paths;
mod repo;
mod routes;
mod state;

use anyhow::{bail, Result};
use axum::{extract::State, routing::get, Json, Router};
use clap::Parser;
use config::{Cli, Command, StartArgs};
use paths::Paths;
use serde::Serialize;
use state::AppState;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    engine: &'static str,
    vec_enabled: bool,
    pid: u32,
    uptime_ms: u64,
}

async fn health(State(app): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        engine: "rust",
        vec_enabled: app.vec_enabled(),
        pid: app.pid(),
        uptime_ms: app.uptime_ms(),
    })
}

fn init_tracing() {
    // CLAWKETD_LOG wins; CLAWKET_DEBUG=1 upgrades default to "debug".
    // Node parity: CLAWKET_DEBUG also flips stack-trace inclusion on error responses
    // (handled in routes/error.rs via env lookup).
    let default = if std::env::var("CLAWKET_DEBUG")
        .ok()
        .filter(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .is_some()
    {
        "debug"
    } else {
        "info"
    };
    let filter = std::env::var("CLAWKETD_LOG").unwrap_or_else(|_| default.to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        None => run_daemon(cli.start).await,
        Some(Command::Start(args)) => run_daemon(args).await,
        Some(Command::Restart(args)) => {
            // Best-effort stop; ignore failures so a cold restart still proceeds.
            let _ = stop_daemon().await;
            run_daemon(args).await
        }
        Some(Command::Stop) => stop_daemon().await,
        Some(Command::Status) => status_daemon().await,
    }
}

async fn run_daemon(args: StartArgs) -> Result<()> {
    let mut paths_cfg = Paths::resolve()?;
    if let Some(db) = &args.db {
        paths_cfg.db = db.clone();
    }
    paths_cfg.ensure_dirs()?;

    let database = db::Db::open(&paths_cfg.db)?;
    tracing::info!(
        vec_enabled = database.vec_enabled,
        "database initialized"
    );
    let app_state = AppState::new(database, paths_cfg.clone());

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let tcp_listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = tcp_listener.local_addr()?;
    paths::write_port_file(&paths_cfg.port_file, bound.port())?;
    paths::write_pid_file(&paths_cfg.pid_file, std::process::id())?;

    paths::prepare_socket_path(&paths_cfg.socket)?;
    let unix_listener = tokio::net::UnixListener::bind(&paths_cfg.socket)?;

    tracing::info!(
        port = bound.port(),
        socket = %paths_cfg.socket.display(),
        db = %paths_cfg.db.display(),
        port_file = %paths_cfg.port_file.display(),
        pid_file = %paths_cfg.pid_file.display(),
        pid = std::process::id(),
        "clawketd listening"
    );

    let app = Router::new()
        .route("/health", get(health))
        .merge(routes::router())
        .with_state(app_state.clone());

    tokio::spawn(backfill_missing_embeddings(app_state.clone()));

    let shutdown_token = tokio_util_cancel::CancelToken::new();
    let shutdown_tcp = shutdown_token.child();
    let shutdown_unix = shutdown_token.child();

    let signal_task = tokio::spawn({
        let token = shutdown_token.clone();
        async move {
            let ctrl_c = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            #[cfg(unix)]
            let terminate = async {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut sig) = signal(SignalKind::terminate()) {
                    sig.recv().await;
                }
            };
            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();
            tokio::select! {
                _ = ctrl_c => {},
                _ = terminate => {},
            }
            tracing::info!("shutdown signal received");
            token.cancel();
        }
    });

    let tcp_fut = axum::serve(tcp_listener, app.clone())
        .with_graceful_shutdown(async move { shutdown_tcp.wait().await });
    let unix_fut = axum::serve(unix_listener, app)
        .with_graceful_shutdown(async move { shutdown_unix.wait().await });

    let port_file = paths_cfg.port_file.clone();
    let pid_file = paths_cfg.pid_file.clone();
    let socket_file = paths_cfg.socket.clone();
    let tcp_task = async move { tcp_fut.await };
    let unix_task = async move { unix_fut.await };
    let result = tokio::try_join!(tcp_task, unix_task);
    let _ = signal_task.await;
    paths::remove_port_file(&port_file);
    paths::remove_pid_file(&pid_file);
    paths::remove_socket_file(&socket_file);
    result?;
    Ok(())
}

async fn stop_daemon() -> Result<()> {
    let paths_cfg = Paths::resolve()?;
    let pid = match paths::read_pid_file(&paths_cfg.pid_file) {
        Some(p) => p,
        None => {
            eprintln!("[clawketd] no pid file at {}", paths_cfg.pid_file.display());
            return Ok(());
        }
    };

    if !process_alive(pid) {
        eprintln!("[clawketd] pid {pid} not alive; cleaning stale pid file");
        paths::remove_pid_file(&paths_cfg.pid_file);
        return Ok(());
    }

    #[cfg(unix)]
    {
        // SIGTERM for graceful shutdown.
        let rc = unsafe { libc_sys::kill(pid as i32, 15) };
        if rc != 0 {
            bail!("kill({pid}, SIGTERM) failed");
        }
    }
    #[cfg(not(unix))]
    {
        bail!("stop subcommand only supported on Unix for now");
    }

    // Poll for exit, up to 10s.
    for _ in 0..100 {
        if !process_alive(pid) {
            paths::remove_pid_file(&paths_cfg.pid_file);
            paths::remove_port_file(&paths_cfg.port_file);
            eprintln!("[clawketd] pid {pid} stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("pid {pid} did not exit within 10s of SIGTERM");
}

async fn status_daemon() -> Result<()> {
    let paths_cfg = Paths::resolve()?;
    let pid = paths::read_pid_file(&paths_cfg.pid_file);
    let port = paths::read_port_file(&paths_cfg.port_file);

    let alive = pid.map(process_alive).unwrap_or(false);
    let health_ok = if let Some(p) = port {
        probe_health(p).await
    } else {
        false
    };

    let body = serde_json::json!({
        "pid": pid,
        "port": port,
        "alive": alive,
        "healthy": health_ok,
        "pid_file": paths_cfg.pid_file.to_string_lossy(),
        "port_file": paths_cfg.port_file.to_string_lossy(),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    if !alive {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) returns 0 if the process exists and we have permission to signal it.
    unsafe { libc_sys::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    // Windows stub — assume alive when pid file exists.
    true
}

async fn probe_health(port: u16) -> bool {
    // Low-level probe without pulling in a full HTTP client: open TCP, send minimal GET.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = format!("127.0.0.1:{port}");
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return false,
    };
    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 512];
    match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.") && head.contains(" 200 ")
        }
        _ => false,
    }
}

// Node v2.2.1 parity: on boot, scan tasks without vec_tasks entries and embed
// them so historical rows become searchable. Runs detached so startup latency
// stays bounded to HTTP bind.
async fn backfill_missing_embeddings(state: AppState) {
    let rows: Vec<(String, String, String)> = {
        let conn = state.conn();
        let sql = "SELECT t.id, t.title, t.body FROM tasks t
                   WHERE NOT EXISTS (SELECT 1 FROM vec_tasks v WHERE v.task_id = t.id)";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(err) => {
                tracing::debug!("backfill prepare failed: {err}");
                return;
            }
        };
        let mapped = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        });
        match mapped {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => return,
        }
    };
    if rows.is_empty() {
        return;
    }
    tracing::info!(count = rows.len(), "backfilling embeddings");
    let mut done = 0usize;
    let mut failed = 0usize;
    for (id, title, body) in rows {
        let source = format!("{title}\n{body}");
        match embeddings::embed(&source).await {
            Ok(Some(vec)) => match repo::tasks::store_embedding(&state.conn(), &id, &vec) {
                Ok(_) => done += 1,
                Err(err) => {
                    tracing::warn!(task_id = %id, "backfill store failed: {err:#}");
                    failed += 1;
                }
            },
            Ok(None) => {
                tracing::debug!(task_id = %id, "backfill skipped: empty source");
                failed += 1;
            }
            Err(err) => {
                tracing::warn!(task_id = %id, "backfill embed failed: {err:#}");
                failed += 1;
            }
        }
    }
    tracing::info!(done, failed, "backfill complete");
}

mod tokio_util_cancel {
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[derive(Clone)]
    pub struct CancelToken {
        inner: Arc<Notify>,
    }

    impl CancelToken {
        pub fn new() -> Self {
            Self { inner: Arc::new(Notify::new()) }
        }

        pub fn child(&self) -> Self {
            self.clone()
        }

        pub fn cancel(&self) {
            self.inner.notify_waiters();
        }

        pub async fn wait(&self) {
            self.inner.notified().await;
        }
    }
}

// Minimal libc bindings for kill(2); avoids pulling in the libc crate for one syscall.
#[cfg(unix)]
mod libc_sys {
    extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}
