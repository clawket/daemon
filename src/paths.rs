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
    pub web_dir: Option<PathBuf>,
    /// FIX-DAEMON-108: TCP auth token file (cache/clawketd.token). Regenerated on each start.
    pub token_file: PathBuf,
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

        // Hard invariant (LM-8): user data must never live under Claude Code's
        // plugin directory. Plugin reinstall (`/plugin install`) wipes that
        // tree, and the post-incident analysis traces our SSoT loss exactly to
        // overlapping paths. Bypass with CLAWKET_ALLOW_PLUGIN_OVERLAP=1 only
        // for tests that explicitly want to assert the unsafe layout.
        let allow_overlap = env::var_os("CLAWKET_ALLOW_PLUGIN_OVERLAP").is_some();
        ensure_no_plugin_overlap("data", &data, allow_overlap)?;
        ensure_no_plugin_overlap("cache", &cache, allow_overlap)?;
        ensure_no_plugin_overlap("config", &config, allow_overlap)?;
        ensure_no_plugin_overlap("state", &state, allow_overlap)?;
        ensure_no_plugin_overlap("db", &db, allow_overlap)?;

        let web_dir = resolve_web_dir();

        Ok(Self {
            port_file: cache.join("clawketd.port"),
            pid_file: cache.join("clawketd.pid"),
            token_file: cache.join("clawketd.token"),
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
        self.warn_legacy_data();
        Ok(())
    }

    // Detect a pre-rebrand Lattice data directory and print a one-shot warning.
    // Migration is intentionally NOT performed: the schema has moved too far across
    // versions, so Clawket treats every install as a fresh start.
    fn warn_legacy_data(&self) {
        const LEGACY_APP: &str = "lattice";
        let legacy_base = if let Some(p) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(p)
        } else if let Ok(home) = home_dir() {
            home.join(".local/share")
        } else {
            return;
        };
        let legacy_db = legacy_base.join(LEGACY_APP).join("db.sqlite");

        if !legacy_db.exists() {
            return;
        }

        eprintln!(
            "[clawket] WARNING: legacy lattice data detected at {}",
            legacy_db.display()
        );
        eprintln!(
            "[clawket] WARNING: migration is NOT supported — schema changed too much across versions."
        );
        eprintln!(
            "[clawket] WARNING: Clawket treats every install as a fresh start. Remove the legacy directory manually if you no longer need it."
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
            // Dev layouts. target/{release,debug}/clawketd → clawket/web/dist lives 3
            // levels up (exe_dir=target/release → ../.. = crate root = daemon repo,
            // ../../.. = clawket workspace root where sibling web/ lives).
            candidates.push(exe_dir.join("../../web/dist"));
            candidates.push(exe_dir.join("../../../web/dist"));
        }
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("web/dist"));
        candidates.push(cwd.join("../web/dist"));
        candidates.push(cwd.join("../../web/dist"));
    }

    candidates
        .into_iter()
        .find(|c| c.join("index.html").is_file())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot resolve XDG paths")
}

fn resolve_dir(override_env: &str, xdg_var: &str, home_fallback: &str) -> Result<PathBuf> {
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

/// FIX-DAEMON-019: Set Unix file permissions to 0600 (owner read/write only).
/// Called after writing port/pid/token files and after binding the socket.
/// No-op on non-Unix targets.
fn set_mode_0600(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn write_port_file(path: &Path, port: u16) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{port}\n"))
        .with_context(|| format!("write port file: {}", path.display()))?;
    set_mode_0600(path);
    Ok(())
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
        .with_context(|| format!("write pid file: {}", path.display()))?;
    set_mode_0600(path);
    Ok(())
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

/// FIX-DAEMON-108: Generate a random 32-byte hex token for TCP auth.
/// Regenerated on every daemon start (token rotation on restart).
pub fn generate_token() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // We don't want to add a new crypto dep. Use combined entropy from PID,
    // current time (nanoseconds), and a stack address for randomness.
    // For a local daemon token this is adequate — not for cryptographic secrets.
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let addr = &pid as *const _ as u64;

    let mut h1 = DefaultHasher::new();
    pid.hash(&mut h1);
    ts.hash(&mut h1);
    addr.hash(&mut h1);
    let v1 = h1.finish();

    let mut h2 = DefaultHasher::new();
    ts.hash(&mut h2);
    addr.hash(&mut h2);
    pid.hash(&mut h2);
    let v2 = h2.finish();

    let mut h3 = DefaultHasher::new();
    addr.hash(&mut h3);
    v1.hash(&mut h3);
    v2.hash(&mut h3);
    let v3 = h3.finish();

    let mut h4 = DefaultHasher::new();
    v2.hash(&mut h4);
    ts.hash(&mut h4);
    v3.hash(&mut h4);
    let v4 = h4.finish();

    format!("{:016x}{:016x}{:016x}{:016x}", v1, v2, v3, v4)
}

/// FIX-DAEMON-108: Write the TCP auth token to the token file (0600).
pub fn write_token_file(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{token}\n"))
        .with_context(|| format!("write token file: {}", path.display()))?;
    set_mode_0600(path);
    Ok(())
}

/// FIX-DAEMON-108: Read the TCP auth token from the token file.
/// Used by CLI to inject token into HTTP requests to the TCP listener.
#[allow(dead_code)]
pub fn read_token_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn remove_token_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// FIX-DAEMON-110: SOCKET_BUSY detection.
/// Before removing a stale socket, check if a previous daemon process is still
/// alive by reading the pid file. If the process is alive, bail with SOCKET_BUSY
/// so the caller (CLI restart logic) can surface the right error.
pub fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket parent dir: {}", parent.display()))?;
    }
    if path.exists() {
        // Attempt to detect if a live process owns this socket via the pid file.
        // pid file lives in the same cache dir as the socket.
        let pid_file = path.with_file_name("clawketd.pid");
        if let Some(pid) = read_pid_file(&pid_file) {
            if process_is_alive(pid) {
                anyhow::bail!(
                    "SOCKET_BUSY: another clawketd process (pid={}) is already listening on {}. \
                     Stop it first with `clawket daemon stop`.",
                    pid,
                    path.display()
                );
            }
        }
        // Either no pid file or process is dead — safe to remove stale socket.
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// FIX-DAEMON-110: Check if a process with the given PID is alive (best-effort).
/// Uses `/proc/<pid>` on Linux; `kill -0` via std::Command on macOS/BSD.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS / BSD: use `kill -0 <pid>` via shell
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// FIX-DAEMON-019: Set socket file to 0600 after bind.
/// The socket is created by `tokio::net::UnixListener::bind`; call this
/// immediately after the bind returns so the window with world-readable
/// permissions is as short as possible.
pub fn set_socket_permissions(path: &Path) {
    set_mode_0600(path);
}

// LM-8 invariant: user data must NEVER resolve under Claude Code's plugin
// directory. Plugin reinstall wipes that tree, so any clawket SQLite/state
// landing there is destroyed silently. This function is the single guard
// called from `Paths::resolve` and re-used by `clawket doctor`.
//
// `allow_overlap` is plumbed in (rather than read from env inline) so tests
// can exercise both branches without env mutation, which would race in the
// default parallel test runner.
pub fn ensure_no_plugin_overlap(label: &str, p: &Path, allow_overlap: bool) -> Result<()> {
    if !path_overlaps_plugin_dir(p) {
        return Ok(());
    }
    if allow_overlap {
        eprintln!(
            "[clawket] WARNING: {label} path {} overlaps with Claude Code plugin dir; \
             CLAWKET_ALLOW_PLUGIN_OVERLAP is set so continuing (data WILL be lost on plugin reinstall).",
            p.display()
        );
        return Ok(());
    }
    anyhow::bail!(
        "{label} path {} overlaps with Claude Code plugin dir (.claude/plugins/). \
         Plugin reinstall will destroy this data. \
         Point CLAWKET_DATA_DIR / XDG_DATA_HOME / etc at a path outside .claude/plugins/, \
         or set CLAWKET_ALLOW_PLUGIN_OVERLAP=1 to acknowledge the data-loss risk.",
        p.display()
    );
}

// Pure path predicate: does this path lie inside any `.claude/plugins/`
// segment? Public so `clawket doctor` (cli crate) can reproduce the check.
pub fn path_overlaps_plugin_dir(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.contains("/.claude/plugins/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plugin_data_overlap() {
        let p = PathBuf::from("/Users/u/.claude/plugins/data/clawket-x/db.sqlite");
        assert!(path_overlaps_plugin_dir(&p));
        let r = ensure_no_plugin_overlap("data", &p, false);
        assert!(r.is_err(), "expected error for plugin-overlap path");
        let err = format!("{:#}", r.unwrap_err());
        assert!(
            err.contains(".claude/plugins/"),
            "error message must mention plugin dir, got: {err}"
        );
        assert!(
            err.contains("CLAWKET_ALLOW_PLUGIN_OVERLAP"),
            "error message must mention bypass env, got: {err}"
        );
    }

    #[test]
    fn rejects_plugin_cache_overlap() {
        let p = PathBuf::from("/Users/u/.claude/plugins/cache/clawket-x");
        assert!(ensure_no_plugin_overlap("cache", &p, false).is_err());
    }

    #[test]
    fn allows_xdg_paths() {
        for safe in [
            "/Users/u/.local/share/clawket",
            "/Users/u/.cache/clawket",
            "/Users/u/.config/clawket",
            "/Users/u/.local/state/clawket",
            "/home/u/.local/share/clawket/db.sqlite",
            "/tmp/clawket-test/data",
        ] {
            let p = PathBuf::from(safe);
            assert!(!path_overlaps_plugin_dir(&p), "false positive: {safe}");
            assert!(
                ensure_no_plugin_overlap("data", &p, false).is_ok(),
                "false positive blocked: {safe}"
            );
        }
    }

    #[test]
    fn allow_overlap_flag_lets_through_with_warning() {
        let p = PathBuf::from("/Users/u/.claude/plugins/data/clawket-x");
        assert!(
            ensure_no_plugin_overlap("data", &p, true).is_ok(),
            "allow_overlap=true should not error"
        );
    }

    #[test]
    fn near_miss_paths_do_not_match() {
        // Paths that contain '.claude' or 'plugins' but NOT the joined segment
        // must not be flagged.
        for ok in [
            "/Users/u/.claude-config/clawket",
            "/Users/u/projects/plugins/clawket-x",
            "/Users/u/.local/share/clawket/.claude.bak",
        ] {
            assert!(
                !path_overlaps_plugin_dir(&PathBuf::from(ok)),
                "false positive: {ok}"
            );
        }
    }
}
