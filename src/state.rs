use crate::db::{Db, SqlitePool, SqlitePooledConn};
use crate::id::now_ms;
use crate::paths::Paths;
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::broadcast;

/// FIX-DAEMON-102: enriched SSE event with entity_type and monotonic id.
/// `change_type` is injected into `data` (not duplicated as a struct field) so
/// wire-format SSE consumers see it without a redundant in-memory copy.
#[derive(Clone, Debug)]
pub struct BroadcastEvent {
    pub event: &'static str,
    /// entity_type extracted from event name (e.g. "task" from "task:updated"),
    /// used by `/events?entity_types=…` server-side filter.
    pub entity_type: &'static str,
    pub data: Value,
    /// monotonic event id for Last-Event-ID / replay
    pub id: u64,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    /// FIX-DAEMON-103 T3: r2d2 connection pool replaces the legacy
    /// `Mutex<Connection>` single-conn model. Every handler that needs a
    /// connection calls `app.conn()` and gets its own pooled `Connection`,
    /// returned to the pool on drop. This restores SQLite WAL multi-reader
    /// concurrency and makes same-thread reentrant locks impossible by
    /// construction.
    pool: SqlitePool,
    paths: Paths,
    vec_enabled: bool,
    schema_version: i64,
    events: broadcast::Sender<BroadcastEvent>,
    started_at: Instant,
    pid: u32,
    /// monotonically increasing event sequence counter
    event_seq: AtomicU64,
    /// US-CKT-SCHEMA-037: while true, mutating routes return HTTP 503 with
    /// code "MIGRATION_IN_PROGRESS". Toggled by the daemon's migration runner
    /// during in-flight schema migrations (current architecture only flips
    /// this for online/runtime migrations, since startup migrations finish
    /// before the listener binds — but the contract is uniform either way).
    migration_in_progress: AtomicBool,
    /// LM-10833: TCP auth token. Same value the tcp_auth middleware validates
    /// against. Held here so the SPA index handler can inject it as an
    /// HttpOnly cookie on first page load — browsers can't read the token
    /// file directly the way the CLI does.
    tcp_token: String,
}

impl AppState {
    pub fn new(db: Db, paths: Paths, tcp_token: String) -> Self {
        let (events, _rx) = broadcast::channel::<BroadcastEvent>(256);
        let schema_version = db.current_schema_version();
        let vec_enabled = db.vec_enabled;
        let pool = db.into_pool();
        Self {
            inner: Arc::new(Inner {
                pool,
                paths,
                vec_enabled,
                schema_version,
                events,
                started_at: Instant::now(),
                pid: std::process::id(),
                event_seq: AtomicU64::new(0),
                migration_in_progress: AtomicBool::new(false),
                tcp_token,
            }),
        }
    }

    /// LM-10833: token used by the SPA cookie bootstrap. Unchanged for the
    /// lifetime of the daemon process (rotated only on restart).
    pub fn tcp_token(&self) -> &str {
        &self.inner.tcp_token
    }

    /// US-CKT-SCHEMA-037: read the current migration gate state.
    pub fn is_migrating(&self) -> bool {
        self.inner.migration_in_progress.load(Ordering::SeqCst)
    }

    /// US-CKT-SCHEMA-037: mark the migration gate. Migration runner must
    /// `set_migrating(true)` before opening a transaction and reset to false
    /// in both success and failure paths (typically via a guard/Drop).
    pub fn set_migrating(&self, v: bool) {
        self.inner.migration_in_progress.store(v, Ordering::SeqCst);
    }

    pub fn uptime_ms(&self) -> u64 {
        self.inner.started_at.elapsed().as_millis() as u64
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// FIX-DAEMON-103 T3: returns an owned pooled connection. Drops back to
    /// the pool when out of scope. Multiple handlers (and even multiple
    /// statements within one handler) can hold independent connections
    /// concurrently — SQLite WAL serializes only writers via its own busy
    /// timeout (set in `Db::new_pool`).
    ///
    /// Panics only if the pool is exhausted within `connection_timeout`
    /// (default 30s) — that would be a sign of a real concurrency runaway,
    /// not a logic bug, and is preferable to a silent deadlock.
    pub fn conn(&self) -> SqlitePooledConn {
        self.inner
            .pool
            .get()
            .expect("db pool exhausted (connection_timeout reached)")
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn vec_enabled(&self) -> bool {
        self.inner.vec_enabled
    }

    pub fn schema_version(&self) -> i64 {
        self.inner.schema_version
    }

    /// FIX-DAEMON-102: emit enriched event with entity_type, change_type, and monotonic id.
    /// event name convention: "entity_type:change_type" (e.g. "task:updated").
    pub fn emit(&self, event: &'static str, mut data: Value) {
        let id = self.inner.event_seq.fetch_add(1, Ordering::Relaxed);
        // Parse entity_type / change_type from event name
        let (entity_type, change_type) = parse_event_name(event);
        // Inject structured fields into data object
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "entity_type".to_string(),
                Value::String(entity_type.to_string()),
            );
            obj.insert(
                "change_type".to_string(),
                Value::String(change_type.to_string()),
            );
            obj.insert("event_id".to_string(), Value::Number(id.into()));
            obj.insert("ts".to_string(), Value::Number(now_ms().into()));
        }
        let _ = self.inner.events.send(BroadcastEvent {
            event,
            entity_type,
            data,
            id,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.inner.events.subscribe()
    }
}

/// Parse "entity:change" event name into (entity_type, change_type).
/// Handles "task:updated", "cycle:activated", etc.
fn parse_event_name(event: &'static str) -> (&'static str, &'static str) {
    // We use a static lookup to keep borrowed lifetimes correct.
    match event {
        "task:created" => ("task", "created"),
        "task:updated" => ("task", "updated"),
        "task:deleted" => ("task", "deleted"),
        "task:started" => ("task", "started"),
        "task:done" => ("task", "done"),
        "task:cancelled" => ("task", "cancelled"),
        "cycle:created" => ("cycle", "created"),
        "cycle:updated" => ("cycle", "updated"),
        "cycle:deleted" => ("cycle", "deleted"),
        "plan:created" => ("plan", "created"),
        "plan:updated" => ("plan", "updated"),
        "plan:deleted" => ("plan", "deleted"),
        "unit:created" => ("unit", "created"),
        "unit:updated" => ("unit", "updated"),
        "unit:deleted" => ("unit", "deleted"),
        "knowledge:created" => ("knowledge", "created"),
        "knowledge:updated" => ("knowledge", "updated"),
        "knowledge:deleted" => ("knowledge", "deleted"),
        "comment:created" => ("comment", "created"),
        "comment:deleted" => ("comment", "deleted"),
        // Every name this daemon actually emits belongs here. An unmapped one
        // falls to ("unknown","unknown"), which is worse than invisible: a client
        // filtering `/events?entity_types=run` receives nothing while runs are
        // being created, so the filter looks like "no activity" rather than
        // "unsupported". The `run:*` and `discover-loop:*` families were in that
        // state; `sse-event-wire-contract.md` names the fallthrough as the hazard.
        //
        // Adding a name here is part of adding an `emit` call, not a follow-up.
        "run:created" => ("run", "created"),
        "run:updated" => ("run", "updated"),
        "discover-loop:started" => ("discover-loop", "started"),
        "discover-loop:active-plan-warning" => ("discover-loop", "active-plan-warning"),
        _ => {
            // Fallback: split on ':'
            if let Some(pos) = event.find(':') {
                // This returns 'static refs only for the fallback — we use a known static slice
                // Since we can't produce 'static from dynamic split, use generic labels.
                let _ = &event[..pos]; // entity
                let _ = &event[pos + 1..]; // change
                ("unknown", "unknown")
            } else {
                ("unknown", event)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_event_name;

    /// Every event name the daemon emits must be mapped. An unmapped name falls to
    /// ("unknown","unknown"), so a client filtering `/events?entity_types=run`
    /// silently receives nothing while runs are happening — the filter reads as "no
    /// activity" rather than "unsupported", and nothing fails loudly.
    ///
    /// This scans the sources for event-name literals rather than restating a list,
    /// so adding an emit without a mapping fails here instead of in production. Two
    /// patterns are covered, because names reach `emit` two ways:
    ///   - `emit("name", …)` — the direct call in the route handlers.
    ///   - `.push(("name", …))` — `repo::tasks::cascade_complete` returns names for
    ///     the route layer to emit through a variable, so those literals sit in a
    ///     file with no `emit(` in it.
    ///
    /// Honest about today's coverage: both cascade names are ALSO emitted directly
    /// elsewhere, so the second pattern adds nothing right now. It is here for the
    /// next name that is only ever returned — verified by deleting the direct sites
    /// in a scratch copy and confirming the scan still finds it.
    ///
    /// What it still cannot see: a name assembled at runtime (`format!`) or read
    /// from a constant. There is none today, and the `!names.is_empty()` guard only
    /// catches a wholesale pattern change, not a single new indirection — so a
    /// future author introducing one must extend this scan.
    ///
    /// It reads files at test time; `CARGO_MANIFEST_DIR` keeps that independent of
    /// the working directory.
    #[test]
    fn every_emitted_event_name_is_mapped() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut names: Vec<String> = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Two marker shapes, and the difference matters:
                //   `emit(`      — the literal may be on a later line (rustfmt wraps
                //                  multi-arg calls), so skip ahead to the next quote.
                //   `.push((\"`  — the marker already consumes the opening quote, so
                //                  the name starts at byte 0. Skipping to "the next
                //                  quote" here lands on the CLOSING one and yields an
                //                  empty string, which is how this marker silently
                //                  contributed nothing when first added.
                for (marker, quote_consumed) in [("emit(", false), (".push((\"", true)] {
                    for chunk in src.split(marker).skip(1) {
                        let rest = if quote_consumed {
                            chunk
                        } else {
                            let Some(open) = chunk.find('"') else {
                                continue;
                            };
                            &chunk[open + 1..]
                        };
                        let Some(close) = rest.find('"') else {
                            continue;
                        };
                        let name = &rest[..close];
                        // Event names are "<entity>:<change>"; anything else is a
                        // different call (e.g. a log macro) and is skipped.
                        if name.contains(':') && !name.contains(' ') && !name.contains('{') {
                            names.push(name.to_string());
                        }
                    }
                }
            }
        }

        assert!(
            !names.is_empty(),
            "scan found no emit() call sites — the pattern must have changed, so this \
             test is no longer checking anything"
        );

        names.sort();
        names.dedup();
        let unmapped: Vec<&String> = names
            .iter()
            .filter(|n| parse_event_name_is_unknown(n))
            .collect();
        assert!(
            unmapped.is_empty(),
            "these emitted events are not mapped in parse_event_name, so they reach \
             subscribers as entity_type=\"unknown\" and cannot be filtered: {unmapped:?}"
        );
    }

    /// `parse_event_name` takes `&'static str`, but scanned names are owned. Match
    /// on the mapped set by round-tripping through the same function with a leaked
    /// string — test-only, and the leak is bounded by the number of event names.
    fn parse_event_name_is_unknown(name: &str) -> bool {
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        parse_event_name(leaked).0 == "unknown"
    }
}
