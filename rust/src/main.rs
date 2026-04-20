mod config;
mod paths;

use anyhow::Result;
use axum::{routing::get, Json, Router};
use clap::Parser;
use config::Cli;
use paths::Paths;
use serde::Serialize;
use std::net::SocketAddr;

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    engine: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        engine: "rust",
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = std::env::var("CLAWKETD_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut paths_cfg = Paths::resolve()?;
    if let Some(db) = &cli.db {
        paths_cfg.db = db.clone();
    }
    paths_cfg.ensure_dirs()?;

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    paths::write_port_file(&paths_cfg.port_file, bound.port())?;

    tracing::info!(
        port = bound.port(),
        db = %paths_cfg.db.display(),
        port_file = %paths_cfg.port_file.display(),
        "clawketd listening"
    );

    let app = Router::new().route("/health", get(health));

    let port_file = paths_cfg.port_file.clone();
    let shutdown = async move {
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
    };

    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown);
    let result = serve.await;
    paths::remove_port_file(&port_file);
    result?;
    Ok(())
}
