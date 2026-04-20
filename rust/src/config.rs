use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clawketd", version, about = "Clawket daemon (Rust)")]
pub struct Cli {
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
