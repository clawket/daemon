mod config;
mod db;
mod decomposition;
mod embeddings;
mod envelope;
mod git;
mod id;
mod import_plan;
mod jobs;
mod locale;
mod middleware;
mod models;
mod paths;
mod repo;
mod routes;
mod secrets;
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
use tower_http::cors::{Any, CorsLayer};

// FIX-DAEMON-020: Health response includes schema_version for status command.
#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    engine: &'static str,
    vec_enabled: bool,
    pid: u32,
    uptime_ms: u64,
    schema_version: i64,
}

async fn health(State(app): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        engine: "rust",
        vec_enabled: app.vec_enabled(),
        pid: app.pid(),
        uptime_ms: app.uptime_ms(),
        schema_version: app.schema_version(),
    })
}

fn init_tracing() {
    // FIX-DAEMON-015: JSON output support.
    // CLAWKETD_LOG_FORMAT=json  → structured JSON (one object per line, machine-parseable).
    // CLAWKETD_LOG wins over CLAWKET_DEBUG for the log level.
    // CLAWKET_DEBUG=1 upgrades default level to "debug".
    // Node parity: CLAWKET_DEBUG also flips stack-trace inclusion on error responses
    // (handled in routes/error.rs via env lookup).
    let json_mode = std::env::var("CLAWKETD_LOG_FORMAT")
        .ok()
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let default_level = if std::env::var("CLAWKET_DEBUG")
        .ok()
        .filter(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .is_some()
    {
        "debug"
    } else {
        "info"
    };
    let filter = std::env::var("CLAWKETD_LOG").unwrap_or_else(|_| default_level.to_string());

    if json_mode {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }
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

    // US-CKT-SCHEMA-038: pass the state dir so migration events are appended
    // to <state>/migration.log in addition to the tracing subscriber.
    let database = db::Db::open_with_state(&paths_cfg.db, Some(paths_cfg.state.clone()))?;
    tracing::info!(vec_enabled = database.vec_enabled, "database initialized");

    // FIX-DAEMON-108: generate the TCP auth token early so AppState can hold a
    // copy. LM-10833: the SPA index handler reads it from AppState to issue the
    // HttpOnly bootstrap cookie. The token is also persisted to the cache file
    // below for the CLI's existing header channel.
    let tcp_token = paths::generate_token();
    let app_state = AppState::new(database, paths_cfg.clone(), tcp_token.clone());

    // TCP-AUTH-V3 / PUBLIC_BIND_NOT_ALLOWED: clawketd is a local-first daemon and the
    // SQLite store is not designed for multi-tenant exposure. Refuse non-loopback binds
    // unless the operator has explicitly opted in via CLAWKET_ALLOW_PUBLIC_BIND=1.
    if !is_loopback_host(&args.host)
        && std::env::var("CLAWKET_ALLOW_PUBLIC_BIND").ok().as_deref() != Some("1")
    {
        anyhow::bail!(
            "PUBLIC_BIND_NOT_ALLOWED: refusing to bind '{}' — clawketd serves local-only data. \
             Use 127.0.0.1 / ::1 / localhost, or set CLAWKET_ALLOW_PUBLIC_BIND=1 to opt in.",
            args.host
        );
    }
    let tcp_listener = bind_with_fallback(&args.host, args.port).await?;
    let bound = tcp_listener.local_addr()?;
    paths::write_port_file(&paths_cfg.port_file, bound.port())?;
    paths::write_pid_file(&paths_cfg.pid_file, std::process::id())?;

    // FIX-DAEMON-108: persist the TCP auth token (rotated on every restart).
    // The token itself was generated above so AppState owns a copy.
    paths::write_token_file(&paths_cfg.token_file, &tcp_token)?;

    paths::prepare_socket_path(&paths_cfg.socket)?;
    let unix_listener = tokio::net::UnixListener::bind(&paths_cfg.socket)?;
    // FIX-DAEMON-019: restrict socket to owner read/write (0600).
    paths::set_socket_permissions(&paths_cfg.socket);

    tracing::info!(
        port = bound.port(),
        socket = %paths_cfg.socket.display(),
        db = %paths_cfg.db.display(),
        port_file = %paths_cfg.port_file.display(),
        pid_file = %paths_cfg.pid_file.display(),
        token_file = %paths_cfg.token_file.display(),
        pid = std::process::id(),
        "clawketd listening"
    );

    let base_router = Router::new()
        .route("/health", get(health))
        .merge(routes::router())
        // US-CKT-SCHEMA-037: 503 with code MIGRATION_IN_PROGRESS for mutating
        // requests while a schema migration is running. Applied at the base
        // layer so both Unix and TCP listeners observe identical behavior.
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            middleware::migration_gate::migration_gate,
        ))
        .with_state(app_state.clone());

    // FIX-DAEMON-108: TCP listener gets auth middleware; Unix socket is exempt (local = trusted)
    let tcp_app = base_router
        .clone()
        .route_layer(axum::middleware::from_fn_with_state(
            tcp_token,
            middleware::tcp_auth::tcp_auth_layer,
        ));
    // Unix socket uses the same routes without the auth layer
    let unix_app = base_router;

    // CORS for dev: web at vite dev server (e.g. http://localhost:5174) needs to
    // talk to the daemon directly when the proxy can't carry SSE without buffering.
    // Daemon binds to loopback only (PUBLIC_BIND_NOT_ALLOWED guard above), so a
    // permissive policy here is scoped to localhost. Applied as the outermost
    // layer so OPTIONS preflight bypasses tcp_auth (no token on preflight).
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);
    let tcp_app = tcp_app.layer(cors);

    tokio::spawn(backfill_missing_embeddings(app_state.clone()));
    tokio::spawn(activity_log_rollup_loop(app_state.clone()));

    // watch channel is level-triggered: a listener that calls `wait_for` after
    // the signal has already been sent still observes it. `tokio::sync::Notify`
    // would be edge-triggered — if cancel() ran before the graceful_shutdown
    // future registered its waiter, SIGTERM would be lost and the daemon would
    // hang forever, manifesting as a `clawket daemon restart` timeout.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tcp = shutdown_rx.clone();
    let shutdown_unix = shutdown_rx.clone();

    let signal_task = tokio::spawn(async move {
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
        let _ = shutdown_tx.send(true);
    });

    let tcp_fut = axum::serve(tcp_listener, tcp_app).with_graceful_shutdown(async move {
        let mut rx = shutdown_tcp;
        let _ = rx.wait_for(|v| *v).await;
    });
    let unix_fut = axum::serve(unix_listener, unix_app).with_graceful_shutdown(async move {
        let mut rx = shutdown_unix;
        let _ = rx.wait_for(|v| *v).await;
    });

    let port_file = paths_cfg.port_file.clone();
    let pid_file = paths_cfg.pid_file.clone();
    let socket_file = paths_cfg.socket.clone();
    let token_file = paths_cfg.token_file.clone();

    // axum's with_graceful_shutdown only stops accepting NEW connections; it
    // waits for existing keep-alive connections to close naturally. A single
    // long-lived client (web dashboard tab, sticky proxy) blocks shutdown
    // indefinitely. After the shutdown signal fires we give in-flight requests
    // a short grace window and then force-exit. Clients can reconnect on the
    // next start; preserving a wedged shutdown is worse than dropping a
    // late-arriving response.
    let serve = async move { tokio::try_join!(tcp_fut, unix_fut) };
    let force_exit_after_grace = async move {
        let mut rx = shutdown_rx;
        let _ = rx.wait_for(|v| *v).await;
        tokio::time::sleep(force_exit_grace()).await;
    };

    tokio::select! {
        res = serve => {
            let _ = signal_task.await;
            paths::remove_port_file(&port_file);
            paths::remove_pid_file(&pid_file);
            paths::remove_socket_file(&socket_file);
            paths::remove_token_file(&token_file);
            res?;
        }
        _ = force_exit_after_grace => {
            tracing::warn!(
                "graceful shutdown grace period expired; forcing exit (clients holding keep-alive)"
            );
            paths::remove_port_file(&port_file);
            paths::remove_pid_file(&pid_file);
            paths::remove_socket_file(&socket_file);
            paths::remove_token_file(&token_file);
            std::process::exit(0);
        }
    }
    Ok(())
}

/// Maximum time to wait for in-flight HTTP requests to drain after a shutdown
/// signal before forcing process exit. Override with CLAWKETD_SHUTDOWN_GRACE_MS
/// (used by integration tests to keep total runtime bounded).
fn force_exit_grace() -> Duration {
    std::env::var("CLAWKETD_SHUTDOWN_GRACE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(3))
}

/// TCP-AUTH-V3: classify a host string as loopback. We accept the common literals
/// (127.0.0.1, ::1, localhost) plus any 127.0.0.0/8 IPv4 address. Anything else —
/// `0.0.0.0`, LAN IPs, public IPs — is treated as a public bind.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Bind a TCP listener on `host:port`. If `port == 0`, OS picks a random port.
/// Otherwise, try `port`, `port+1`, ... for up to 20 attempts, skipping ports
/// already in use. Fails if no port in the range is free.
async fn bind_with_fallback(host: &str, port: u16) -> Result<tokio::net::TcpListener> {
    if port == 0 {
        let addr: SocketAddr = format!("{host}:0").parse()?;
        return Ok(tokio::net::TcpListener::bind(addr).await?);
    }

    const MAX_ATTEMPTS: u16 = 20;
    let mut last_err: Option<std::io::Error> = None;
    for offset in 0..MAX_ATTEMPTS {
        let candidate = port.saturating_add(offset);
        let addr: SocketAddr = format!("{host}:{candidate}").parse()?;
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if offset > 0 {
                    tracing::warn!(
                        requested = port,
                        bound = candidate,
                        "port in use; bound to next free port"
                    );
                }
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let end = port.saturating_add(MAX_ATTEMPTS - 1);
    bail!(
        "no free port in range {port}..={end}: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into())
    )
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

    // FIX-DAEMON-020: include socket path, uptime_ms (from /health probe), schema_version.
    // uptime_ms: try to read from /health endpoint if daemon is alive.
    let (uptime_ms, schema_version) = if health_ok {
        if let Some(p) = port {
            fetch_health_fields(p).await
        } else {
            (None, None)
        }
    } else {
        // Try to read schema version directly from DB (daemon may be stopped).
        let sv = paths_cfg
            .db
            .exists()
            .then(|| {
                db::Db::open(&paths_cfg.db)
                    .ok()
                    .map(|d| d.current_schema_version())
            })
            .flatten();
        (None, sv)
    };

    let body = serde_json::json!({
        "pid": pid,
        "port": port,
        "socket": paths_cfg.socket.to_string_lossy(),
        "alive": alive,
        "healthy": health_ok,
        "uptime_ms": uptime_ms,
        "schema_version": schema_version,
        "pid_file": paths_cfg.pid_file.to_string_lossy(),
        "port_file": paths_cfg.port_file.to_string_lossy(),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    if !alive {
        std::process::exit(1);
    }
    Ok(())
}

/// FIX-DAEMON-020: Read uptime_ms and schema_version from the live daemon's /health endpoint.
async fn fetch_health_fields(port: u16) -> (Option<u64>, Option<i64>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = format!("127.0.0.1:{port}");
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return (None, None),
    };
    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .is_err()
    {
        return (None, None);
    }
    let mut buf = vec![0u8; 4096];
    let n = match tokio::time::timeout(Duration::from_secs(1), stream.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => return (None, None),
    };
    let raw = String::from_utf8_lossy(&buf[..n]);
    // Find JSON body after double CRLF.
    if let Some(pos) = raw.find("\r\n\r\n") {
        let json_body = &raw[pos + 4..];
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_body) {
            let uptime = val.get("uptime_ms").and_then(|v| v.as_u64());
            let sv = val.get("schema_version").and_then(|v| v.as_i64());
            return (uptime, sv);
        }
    }
    (None, None)
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
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
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

// activity_log retention loop (RL-U3-16, ADR-0010). Runs once at startup
// (catch up archive lag for users who left the daemon off for a week) then
// every 24h. Failures are logged and swallowed — a stuck rollup must never
// take the daemon down.
async fn activity_log_rollup_loop(state: AppState) {
    let policy = jobs::activity_log_rollup::RetentionPolicy::from_env();
    tracing::info!(
        hot_days = policy.hot_days,
        total_days = policy.total_days,
        max_mb = policy.max_mb,
        "activity_log retention policy loaded"
    );
    loop {
        let result = {
            let mut conn = state.conn();
            jobs::activity_log_rollup::run_once(&mut conn, &policy, id::now_ms())
        };
        match result {
            Ok(report) => tracing::info!(
                batches = report.batches_archived,
                rows_archived = report.rows_archived,
                rows_pruned = report.rows_cold_pruned,
                effective_total_days = report.effective_total_days,
                size_bytes = report.size_bytes_after,
                over_budget = report.over_budget,
                "activity_log rollup complete"
            ),
            Err(e) => tracing::warn!(error = %e, "activity_log rollup failed"),
        }
        // 24h between passes. Tests don't reach this branch — they call
        // run_once directly.
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

// Minimal libc bindings for kill(2); avoids pulling in the libc crate for one syscall.
#[cfg(unix)]
mod libc_sys {
    extern "C" {
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}
