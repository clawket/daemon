use crate::db::Db;
use crate::paths::Paths;
use rusqlite::Connection;
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct BroadcastEvent {
    pub event: &'static str,
    pub data: Value,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    paths: Paths,
    vec_enabled: bool,
    events: broadcast::Sender<BroadcastEvent>,
    started_at: Instant,
    pid: u32,
}

impl AppState {
    pub fn new(db: Db, paths: Paths) -> Self {
        let (events, _rx) = broadcast::channel::<BroadcastEvent>(256);
        Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(db.conn),
                paths,
                vec_enabled: db.vec_enabled,
                events,
                started_at: Instant::now(),
                pid: std::process::id(),
            }),
        }
    }

    pub fn uptime_ms(&self) -> u64 {
        self.inner.started_at.elapsed().as_millis() as u64
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.inner.conn.lock().expect("db mutex poisoned")
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn vec_enabled(&self) -> bool {
        self.inner.vec_enabled
    }

    pub fn emit(&self, event: &'static str, data: Value) {
        let _ = self.inner.events.send(BroadcastEvent { event, data });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastEvent> {
        self.inner.events.subscribe()
    }
}
