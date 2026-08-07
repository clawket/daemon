//! Daemon-side CLI ↔ route contract gate (always-on, blocking).
//!
//! The CLI and the daemon ship from separate repos on separate release
//! cadences, so nothing stops a `clawket` command from being released against
//! an HTTP path the daemon never implemented. That is exactly how `backup`,
//! `restore`, `update`, `version-check`, `config` and `migrate` shipped dead
//! from the v3.0.0 baseline: the CLI forwarded to paths absent from
//! `routes/mod.rs::router()`, and users got `unknown error` (or a JSON parse
//! failure on the empty 404 body) with no hint the endpoint was missing.
//!
//! `cli/tests/daemon_route_contract.rs` closes the gap from the CLI side, but
//! it probes whatever daemon happens to be running — usually an installed
//! release, not the tree under development — so it is opt-in behind
//! `CLAWKET_ROUTE_CONTRACT_SOCKET` and non-blocking in CI. This test is its
//! blocking counterpart: it spawns a daemon **from the source under test**, so
//! the verdict is a property of this commit and can never be out of phase with
//! what the repo is about to release.
//!
//! # Why spawn a daemon instead of asking the Router directly
//!
//! Interrogating `routes::mod::router()` in-process would be the sturdier
//! check — no ports, no processes. It is not available: `clawketd` is a binary
//! crate with no `src/lib.rs` (`Cargo.toml` declares only `[[bin]]`), so
//! integration tests under `tests/` cannot name `crate::routes`. Adding a lib
//! target purely for this test would restructure the crate, which is out of
//! scope for a gate. Spawning the binary is what every other integration test
//! here does (`http_integration.rs`, `backup_restore.rs`), and it costs one
//! process for the whole file because all probes share a single daemon.
//!
//! Parsing `.route("…")` literals out of the source was rejected outright: it
//! would assert against text rather than behaviour and would miss exactly the
//! failure this gate exists to catch — a router that compiles but never merges
//! a sub-router into `router()`.
//!
//! # Which repo owns the path list
//!
//! The CLI's PROBES table and the table below are deliberately NOT shared.
//! Option B — parsing `../cli/tests/daemon_route_contract.rs` at test time —
//! gives a single source of truth but pays for it with a sibling-checkout
//! dependency: in CI, and in any clone of this repo alone, `../cli` does not
//! exist, so the gate would skip. A gate that skips in CI is the thing this
//! file was written to replace, and "always-on" is worth more than "not
//! duplicated". So this table is independent and intentionally redundant; the
//! two gates cross-check each other, and a disagreement between them is itself
//! the signal that a command changed without its counterpart.
//!
//! # Probe method
//!
//! Send a request with a method the route does not implement and read the
//! status. Axum answers `405` when the PATH is registered but the METHOD is
//! not, and `404` when the path itself is unknown — so 404 means missing. The
//! method is chosen to be one each route rejects, which keeps every probe free
//! of side effects.
//!
//! One wrinkle makes the raw status insufficient. A registered handler may
//! itself return 404 for a nonexistent entity (`{"error":"Task not found"}`),
//! which is a *healthy* route. The two are told apart by the body: axum's
//! built-in fallback for an unmatched path returns 404 with an EMPTY body,
//! while a handler's 404 carries a JSON error. `static_files.rs::not_found()`
//! is a helper called inside handlers, not a router-level `.fallback()`, so
//! nothing else occupies the unmatched-path slot. Probes therefore assert on
//! (status, body-empty) together.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Paths the CLI forwards to, paired with a method the daemon must reject.
///
/// Derived from every `client::get` / `client::request` / `client::request_raw`
/// call site in `cli/src/main.rs`. Query strings are dropped (they do not
/// affect routing) and `{id}`-style segments are filled with a placeholder that
/// will not exist in the fresh database — routing is what is under test, not
/// the entity lookup.
///
/// A command whose path is absent from this table is a command no gate
/// protects. When adding a CLI command that calls the daemon, add its path
/// here in the same change set.
const PROBES: &[(&str, &str, &str)] = &[
    // (cli command, path, method the route must reject with 405)
    //
    // `update`, `version-check` and `config` are deliberately absent: they no
    // longer call the daemon at all. Each was reimplemented locally — GitHub
    // Releases lookup and XDG config file I/O respectively — so there is no
    // route for them to depend on. `migrate` reads `/health`, covered below.
    ("backup", "/backup", "DELETE"),
    ("restore", "/restore", "DELETE"),
    ("migrate", "/health", "DELETE"),
    ("dashboard", "/dashboard", "DELETE"),
    ("project list", "/projects", "DELETE"),
    ("project by-cwd", "/projects/by-cwd/tmp", "DELETE"),
    ("plan list", "/plans", "DELETE"),
    ("plan view", "/plans/PLAN-x", "POST"),
    ("plan approve", "/plans/PLAN-x/approve", "DELETE"),
    ("plan import", "/plans/import", "DELETE"),
    ("plan import --strict", "/plans/import/strict", "DELETE"),
    ("unit list", "/units", "DELETE"),
    ("unit counts", "/units/UNIT-x/counts", "DELETE"),
    ("cycle list", "/cycles", "DELETE"),
    ("cycle activate", "/cycles/CYCLE-x/activate", "DELETE"),
    ("cycle complete", "/cycles/CYCLE-x/complete", "DELETE"),
    ("cycle counts", "/cycles/CYCLE-x/counts", "DELETE"),
    ("task list", "/tasks", "DELETE"),
    ("task search", "/tasks/search", "DELETE"),
    ("task stats", "/tasks/stats", "DELETE"),
    ("task ancestors", "/tasks/TASK-x/ancestors", "DELETE"),
    ("task descendants", "/tasks/TASK-x/descendants", "DELETE"),
    ("task drift", "/tasks/TASK-x/drift", "DELETE"),
    ("task envelope", "/tasks/TASK-x/envelope", "POST"),
    ("knowledge list/add", "/knowledge", "DELETE"),
    ("knowledge search", "/knowledge/search", "DELETE"),
    ("knowledge import", "/knowledge/import", "DELETE"),
    ("knowledge export", "/knowledge/export", "DELETE"),
    ("wiki", "/wiki/tree", "DELETE"),
    ("run list", "/runs", "DELETE"),
    ("question list", "/questions", "DELETE"),
    ("comment list", "/comments", "DELETE"),
    ("events", "/events/replay", "DELETE"),
    ("discover start", "/discover-loop/start", "DELETE"),
    ("discover next-round", "/discover-loop/next-round", "DELETE"),
    (
        "discover dispatch-plan",
        "/discover-loop/dispatch-plan",
        "DELETE",
    ),
    ("discover verify-tsv", "/discover-loop/verify-tsv", "DELETE"),
    ("discover batch-id", "/discover-loop/batch-id", "DELETE"),
    ("discover sync", "/discover-loop/sync", "DELETE"),
    ("discover status", "/discover-loop/status", "DELETE"),
    ("discover converged", "/discover-loop/converged", "DELETE"),
    ("discover rounds", "/discover-loop/rounds/PROJ-x", "DELETE"),
];

/// CLI commands that are broken against this daemon, probed with the method and
/// path the CLI really sends. Reported on every run but not failed, so the gate
/// is landable ahead of the fixes.
///
/// Two distinct failure modes live here, and the difference matters when fixing:
///
/// - `Absent` — nothing matches the path; axum's fallback answers 404 with an
///   empty body.
/// - `Shadowed` — a *different* route matches first, so the request reaches the
///   wrong handler. `/tasks/similar` lands in `get_one` with `id="similar"` and
///   comes back as a missing task. These never look absent from the outside,
///   which is exactly why the plain 404 probe in PROBES cannot police them: the
///   fix is a distinct path, not merely "add a route".
///
/// This is a debt ledger, not a parking lot: every entry is a user-visible
/// broken command, and removing entries is the point. Nothing may be ADDED here
/// for a newly written command — a new command that needs a route gets a route.
const KNOWN_BROKEN: &[Broken] = &[
    Broken {
        cmd: "get-task-context",
        method: "GET",
        path: "/tasks/TASK-x/context",
        mode: Mode::Absent,
        why: "no route at all; the MCP surface serves task context instead, so \
              the CLI command is a candidate for removal",
    },
    Broken {
        cmd: "find-similar-tasks",
        method: "GET",
        path: "/tasks/similar",
        mode: Mode::Shadowed,
        why: "shadowed by `/tasks/{id}` → reaches get_one with id=\"similar\". \
              The implemented route is per-task: `/tasks/{id}/similar`",
    },
    Broken {
        cmd: "replay",
        method: "GET",
        path: "/runs/replay",
        mode: Mode::Shadowed,
        why: "shadowed by `/runs/{id}` → reaches get_one with id=\"replay\". \
              The implemented route is `/entities/{id}/replay`",
    },
    Broken {
        cmd: "summary",
        method: "GET",
        path: "/dashboard/summary",
        mode: Mode::Absent,
        why: "`/dashboard` serves every view through `?show=`; the four \
              sub-paths were never registered",
    },
    Broken {
        cmd: "board",
        method: "GET",
        path: "/dashboard/board",
        mode: Mode::Absent,
        why: "see /dashboard/summary",
    },
    Broken {
        cmd: "timeline",
        method: "GET",
        path: "/dashboard/timeline",
        mode: Mode::Absent,
        why: "see /dashboard/summary",
    },
    Broken {
        cmd: "wiki",
        method: "GET",
        path: "/dashboard/wiki",
        mode: Mode::Absent,
        why: "see /dashboard/summary",
    },
];

#[derive(PartialEq)]
enum Mode {
    /// No route matches; axum's fallback replies 404 with an empty body.
    Absent,
    /// Another route matches first and the wrong handler answers.
    Shadowed,
}

struct Broken {
    cmd: &'static str,
    method: &'static str,
    path: &'static str,
    mode: Mode,
    why: &'static str,
}

struct Daemon {
    child: Child,
    socket: std::path::PathBuf,
    _tmpdir: tempfile::TempDir,
}

impl Daemon {
    /// Spawn a daemon from the binary built out of the source under test,
    /// fully isolated in a tempdir. Mirrors `http_integration.rs::spawn`.
    fn spawn() -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmpdir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");

        let child = Command::new(env!("CARGO_BIN_EXE_clawketd"))
            .arg("--port")
            .arg("0")
            .arg("--db")
            .arg(tmpdir.path().join("test.sqlite"))
            .env("CLAWKET_DATA_DIR", tmpdir.path().join("data"))
            .env("CLAWKET_CACHE_DIR", &cache_dir)
            .env("CLAWKET_CONFIG_DIR", tmpdir.path().join("config"))
            .env("CLAWKET_STATE_DIR", tmpdir.path().join("state"))
            // Pin web dir to a nonexistent path so resolve_web_dir() cannot
            // fall back to a sibling workspace build.
            .env("CLAWKET_WEB_DIR", tmpdir.path().join("no-web"))
            .env("CLAWKETD_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn clawketd");

        // Probe over the Unix socket: it is exempt from TCP token auth, so no
        // token plumbing is needed and a 401 can never be mistaken for a 404.
        let socket = cache_dir.join("clawketd.sock");
        // Build Self before the wait loop so `Drop` owns the child on every
        // exit path — panicking here would otherwise orphan a live daemon.
        let daemon = Self {
            child,
            socket,
            _tmpdir: tmpdir,
        };
        for _ in 0..100 {
            if daemon.socket.exists()
                && std::os::unix::net::UnixStream::connect(&daemon.socket).is_ok()
            {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "daemon did not create a usable socket at {} within 10s",
            daemon.socket.display()
        );
    }

    /// Returns (status, body) for a request the route is expected to reject.
    /// Shells out to curl rather than adding an HTTP client that speaks Unix
    /// sockets to dev-dependencies — this test is about the wire contract, and
    /// curl speaks it at no build cost.
    fn probe(&self, method: &str, path: &str) -> Option<(u16, String)> {
        let out = Command::new("curl")
            .args([
                "-s",
                "-w",
                "\n%{http_code}",
                "--max-time",
                "10",
                "-X",
                method,
                "--unix-socket",
            ])
            .arg(&self.socket)
            .arg(format!("http://localhost{path}"))
            .output()
            .ok()?;
        let raw = String::from_utf8_lossy(&out.stdout);
        let (body, status) = raw.rsplit_once('\n')?;
        Some((status.trim().parse().ok()?, body.trim().to_string()))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A 404 from axum's unmatched-path fallback has an empty body; a 404 raised by
/// a real handler carries a JSON error. Only the former means "route absent".
fn is_route_absent(status: u16, body: &str) -> bool {
    status == 404 && body.is_empty()
}

#[test]
fn every_cli_path_is_registered_in_the_daemon_router() {
    let daemon = Daemon::spawn();

    let mut missing: Vec<String> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();

    for (cmd, path, method) in PROBES {
        let Some((status, body)) = daemon.probe(method, path) else {
            unreachable.push(format!("{path} (probe failed to run)"));
            continue;
        };
        if status == 0 {
            unreachable.push(format!("{path} (no response)"));
            continue;
        }
        if is_route_absent(status, &body) {
            missing.push(format!(
                "`clawket {cmd}` → {method} {path} returned 404 with an empty body (route absent)"
            ));
        }
    }

    assert!(
        unreachable.is_empty(),
        "the probe harness could not reach the daemon it spawned, so no verdict \
         below is meaningful: {}",
        unreachable.join(", ")
    );

    assert!(
        missing.is_empty(),
        "These CLI commands forward to daemon paths that `routes/mod.rs::router()` \
         does not register. Every one of them is dead on arrival for users:\n  {}\n\n\
         Fix in ONE of two ways:\n  \
         (a) implement the route in daemon/src/routes/<area>.rs and merge its \
         router() into routes/mod.rs::router() — do this when the command is \
         meant to work; or\n  \
         (b) remove the command from cli/src/main.rs, and drop its row from \
         PROBES here and from cli/tests/daemon_route_contract.rs — do this when \
         the command is obsolete or has been reimplemented locally.\n  \
         Do NOT silence this by moving the row into KNOWN_BROKEN; that list is \
         a shrinking ledger of pre-existing breakage, not a place to park new \
         regressions.",
        missing.join("\n  ")
    );
}

/// Companion to the gate above: the commands already known to be broken.
///
/// Kept separate rather than folded into the gate so the debt stays visible on
/// every run (`cargo test -- --nocapture`) without turning the build red for
/// breakage that predates this file. It fails only when an entry has been
/// FIXED — the prompt to delete the row and let the real gate protect it from
/// then on.
///
/// Each entry is probed with the method and path the CLI actually sends, and
/// checked against its declared mode: `Absent` must yield an empty-bodied 404,
/// `Shadowed` must NOT (some other handler answers). A row whose mode no longer
/// describes reality is as much a drift signal as one that got fixed.
#[test]
fn known_broken_commands_are_still_broken() {
    let daemon = Daemon::spawn();
    let mut fixed: Vec<String> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();

    eprintln!(
        "\nKNOWN GAPS — CLI commands that do not work against this daemon ({} total):",
        KNOWN_BROKEN.len()
    );
    for b in KNOWN_BROKEN {
        let mode = match b.mode {
            Mode::Absent => "absent",
            Mode::Shadowed => "shadowed",
        };
        eprintln!(
            "  - `clawket {}` → {} {} [{mode}]\n      {}",
            b.cmd, b.method, b.path, b.why
        );
    }
    eprintln!();

    for b in KNOWN_BROKEN {
        let Some((status, body)) = daemon.probe(b.method, b.path) else {
            continue;
        };
        let absent = is_route_absent(status, &body);
        match b.mode {
            // Nothing may match the path.
            Mode::Absent if !absent => mismatched.push(format!(
                "`clawket {}` → {} {} is listed as absent but answered {status} \
                 with body {body:?} — something now matches that path",
                b.cmd, b.method, b.path
            )),
            // Something matches, but it must still be the wrong handler: a 2xx
            // means the command genuinely works now.
            Mode::Shadowed if absent => mismatched.push(format!(
                "`clawket {}` → {} {} is listed as shadowed but nothing matches \
                 it any more (empty-bodied 404) — the shadowing route changed",
                b.cmd, b.method, b.path
            )),
            Mode::Shadowed if (200..300).contains(&status) => fixed.push(format!(
                "`clawket {}` → {} {} (now {status})",
                b.cmd, b.method, b.path
            )),
            _ => {}
        }
        if b.mode == Mode::Absent && (200..300).contains(&status) {
            fixed.push(format!(
                "`clawket {}` → {} {} (now {status})",
                b.cmd, b.method, b.path
            ));
        }
    }

    assert!(
        mismatched.is_empty(),
        "KNOWN_BROKEN rows no longer describe how the daemon behaves:\n  {}\n\n\
         Re-probe the path and either correct the row's `mode` or remove it.",
        mismatched.join("\n  ")
    );

    assert!(
        fixed.is_empty(),
        "These commands are listed in KNOWN_BROKEN but the daemon now serves \
         them:\n  {}\n\n\
         That is good news. Delete each row from KNOWN_BROKEN and add its path \
         to PROBES so the always-on gate keeps it working from here on.",
        fixed.join("\n  ")
    );
}
