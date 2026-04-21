// XDG Base Directory Specification with env overrides.
// All runtime paths flow through this module, matching Node @clawket/daemon semantics.
//
// Env overrides (highest precedence):
//   CLAWKET_DATA_DIR    — persistent data (db, attachments)
//   CLAWKET_CACHE_DIR   — regenerable state (socket, pid, tmp)
//   CLAWKET_CONFIG_DIR  — user config (toml/json)
//   CLAWKET_STATE_DIR   — logs, history (XDG state)
//
// Defaults (XDG-hardcoded, platform-independent):
//   data    ← $XDG_DATA_HOME   or ~/.local/share
//   cache   ← $XDG_CACHE_HOME  or ~/.cache
//   config  ← $XDG_CONFIG_HOME or ~/.config
//   state   ← $XDG_STATE_HOME  or ~/.local/state

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
    pub socket: PathBuf,
    pub log_file: PathBuf,
    pub config_file: PathBuf,
    pub web_dir: Option<PathBuf>,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let data = resolve_dir("CLAWKET_DATA_DIR", "XDG_DATA_HOME", ".local/share")?;
        let cache = resolve_dir("CLAWKET_CACHE_DIR", "XDG_CACHE_HOME", ".cache")?;
        let config = resolve_dir("CLAWKET_CONFIG_DIR", "XDG_CONFIG_HOME", ".config")?;
        let state = resolve_dir("CLAWKET_STATE_DIR", "XDG_STATE_HOME", ".local/state")?;

        let db = env::var_os("CLAWKET_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("db.sqlite"));

        let socket = env::var_os("CLAWKET_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| cache.join("clawketd.sock"));

        let web_dir = resolve_web_dir();

        Ok(Self {
            port_file: cache.join("clawketd.port"),
            pid_file: cache.join("clawketd.pid"),
            log_file: state.join("clawketd.log"),
            config_file: config.join("config.toml"),
            socket,
            data,
            cache,
            config,
            state,
            db,
            web_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for p in [&self.data, &self.cache, &self.config, &self.state] {
            std::fs::create_dir_all(p)
                .with_context(|| format!("failed to create directory: {}", p.display()))?;
        }
        self.migrate_legacy_data();
        Ok(())
    }

    // Migrate data from legacy Lattice paths (pre-rebrand) to Clawket paths.
    // Only runs when legacy db.sqlite exists and current db does not. The legacy
    // file is renamed with a `.migrated-to-clawket` suffix to avoid re-migration.
    fn migrate_legacy_data(&self) {
        const LEGACY_APP: &str = "lattice";
        let legacy_base = if let Some(p) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(p)
        } else if let Ok(home) = home_dir() {
            home.join(".local/share")
        } else {
            return;
        };
        let legacy_db = legacy_base.join(LEGACY_APP).join("db.sqlite");

        if !legacy_db.exists() || self.db.exists() {
            return;
        }

        if let Some(parent) = self.db.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "[clawket] WARNING: failed to create {}: {}",
                    parent.display(),
                    e
                );
                return;
            }
        }

        if let Err(e) = std::fs::copy(&legacy_db, &self.db) {
            eprintln!(
                "[clawket] WARNING: failed to migrate legacy database: {}",
                e
            );
            return;
        }

        let marker = legacy_db.with_extension("sqlite.migrated-to-clawket");
        if let Err(e) = std::fs::rename(&legacy_db, &marker) {
            eprintln!(
                "[clawket] WARNING: migrated DB copied but failed to rename legacy: {}",
                e
            );
            return;
        }

        eprintln!(
            "[clawket] Migrated database from {} -> {}",
            legacy_db.display(),
            self.db.display()
        );
    }
}

fn resolve_web_dir() -> Option<PathBuf> {
    if let Some(p) = env::var_os("CLAWKET_WEB_DIR") {
        let path = PathBuf::from(p);
        if path.join("index.html").is_file() {
            return Some(path);
        }
        return None;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Installed layout: bin next to web/.
            candidates.push(exe_dir.join("web"));
            candidates.push(exe_dir.join("../web"));
            // Dev layouts. target/{release,debug}/clawketd → clawket/web/dist lives 4
            // levels up (exe_dir=target/release → ../.. = crate root, ../../.. = parent
            // repo dir, ../../../.. = clawket workspace root where web/ lives).
            candidates.push(exe_dir.join("../../web/dist"));
            candidates.push(exe_dir.join("../../../web/dist"));
            candidates.push(exe_dir.join("../../../../web/dist"));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("web/dist"));
        candidates.push(cwd.join("../web/dist"));
        candidates.push(cwd.join("../../web/dist"));
    }

    for c in candidates {
        if c.join("index.html").is_file() {
            return Some(c);
        }
    }
    None
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot resolve XDG paths")
}

fn resolve_dir(
    override_env: &str,
    xdg_var: &str,
    home_fallback: &str,
) -> Result<PathBuf> {
    if let Some(p) = env::var_os(override_env) {
        return Ok(PathBuf::from(p));
    }
    let base = if let Some(p) = env::var_os(xdg_var) {
        PathBuf::from(p)
    } else {
        home_dir()?.join(home_fallback)
    };
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

pub fn remove_socket_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{pid}\n"))
        .with_context(|| format!("write pid file: {}", path.display()))
}

pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

pub fn read_port_file(path: &Path) -> Option<u16> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

pub fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create socket parent dir: {}", parent.display())
        })?;
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}
