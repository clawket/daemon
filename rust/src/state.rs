use crate::db::Db;
use crate::paths::Paths;
use rusqlite::Connection;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    paths: Paths,
    vec_enabled: bool,
}

impl AppState {
    pub fn new(db: Db, paths: Paths) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(db.conn),
                paths,
                vec_enabled: db.vec_enabled,
            }),
        }
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
}
