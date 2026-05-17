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
            obj.insert("entity_type".to_string(), Value::String(entity_type.to_string()));
            obj.insert("change_type".to_string(), Value::String(change_type.to_string()));
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
        "task:created"   => ("task",    "created"),
        "task:updated"   => ("task",    "updated"),
        "task:deleted"   => ("task",    "deleted"),
        "task:started"   => ("task",    "started"),
        "task:done"      => ("task",    "done"),
        "task:cancelled" => ("task",    "cancelled"),
        "cycle:created"  => ("cycle",   "created"),
        "cycle:updated"  => ("cycle",   "updated"),
        "cycle:deleted"  => ("cycle",   "deleted"),
        "plan:created"   => ("plan",    "created"),
        "plan:updated"   => ("plan",    "updated"),
        "plan:deleted"   => ("plan",    "deleted"),
        "unit:created"   => ("unit",    "created"),
        "unit:updated"   => ("unit",    "updated"),
        "unit:deleted"   => ("unit",    "deleted"),
        "knowledge:created" => ("knowledge", "created"),
        "knowledge:updated" => ("knowledge", "updated"),
        "knowledge:deleted" => ("knowledge", "deleted"),
        "comment:created"  => ("comment",  "created"),
        "comment:deleted"  => ("comment",  "deleted"),
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
