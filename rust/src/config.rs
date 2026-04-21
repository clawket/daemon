use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clawketd", version, about = "Clawket daemon (Rust)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub start: StartArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the daemon in the foreground (default when no subcommand given).
    Start(StartArgs),
    /// Signal a running daemon to shut down gracefully.
    Stop,
    /// Report whether a daemon is running (reads pid+port files, probes /health).
    Status,
    /// Stop the running daemon (if any) then start a new one in the foreground.
    Restart(StartArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct StartArgs {
    /// Port to bind (0 = random, default)
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Override database path
    #[arg(long)]
    pub db: Option<PathBuf>,
}
