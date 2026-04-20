use anyhow::{Context, Result};
use std::env;
use std::path::{Path, PathBuf};

const APP: &str = "clawket";

#[derive(Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub db: PathBuf,
    pub port_file: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let data = resolve_dir("CLAWKET_DATA_DIR", dirs::data_dir, ".local/share")?;
        let cache = resolve_dir("CLAWKET_CACHE_DIR", dirs::cache_dir, ".cache")?;
        let config = resolve_dir("CLAWKET_CONFIG_DIR", dirs::config_dir, ".config")?;
        let state = resolve_dir("CLAWKET_STATE_DIR", dirs::state_dir, ".local/state")?;

        let db = env::var_os("CLAWKET_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("db.sqlite"));

        Ok(Self {
            port_file: cache.join("clawketd.port"),
            pid_file: cache.join("clawketd.pid"),
            log_file: state.join("clawketd.log"),
            config_file: config.join("config.toml"),
            data,
            cache,
            config,
            state,
            db,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for p in [&self.data, &self.cache, &self.config, &self.state] {
            std::fs::create_dir_all(p)
                .with_context(|| format!("failed to create directory: {}", p.display()))?;
        }
        Ok(())
    }
}

fn resolve_dir(
    override_env: &str,
    xdg: impl FnOnce() -> Option<PathBuf>,
    home_fallback: &str,
) -> Result<PathBuf> {
    if let Some(p) = env::var_os(override_env) {
        return Ok(PathBuf::from(p));
    }
    let base = xdg().or_else(|| dirs::home_dir().map(|h| h.join(home_fallback)));
    let base = base.context("could not resolve XDG or home directory")?;
    Ok(base.join(APP))
}

pub fn write_port_file(path: &Path, port: u16) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{port}\n"))
        .with_context(|| format!("write port file: {}", path.display()))
}

pub fn remove_port_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}
