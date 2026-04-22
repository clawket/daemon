use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "001_initial.sql", include_str!("../migrations/001_initial.sql")),
];

pub struct Db {
    pub conn: Connection,
    pub vec_enabled: bool,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir: {}", parent.display()))?;
        }

        let vec_enabled = register_sqlite_vec();

        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let mut db = Self { conn, vec_enabled };
        db.migrate()?;
        if db.vec_enabled {
            db.ensure_vector_tables()?;
        }
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.pragma_update(None, "foreign_keys", "OFF")?;

        let has_table: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        let current: i64 = if has_table {
            self.conn
                .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))
                .unwrap_or(0)
        } else {
            0
        };

        for (version, file, sql) in MIGRATIONS {
            if *version <= current {
                continue;
            }
            let tx = self.conn.transaction()?;
            match tx.execute_batch(sql) {
                Ok(_) => {
                    tx.execute(
                        "INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                        rusqlite::params![version, now_ms()],
                    )?;
                    tx.commit()
                        .with_context(|| format!("commit migration {file}"))?;
                    tracing::info!(version, file, "migration applied");
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("duplicate column") {
                        tx.rollback().ok();
                        self.conn.execute(
                            "INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                            rusqlite::params![version, now_ms()],
                        )?;
                        tracing::warn!(version, file, "migration already applied (duplicate column)");
                    } else {
                        return Err(e).with_context(|| format!("migration {file}"));
                    }
                }
            }
        }

        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    fn ensure_vector_tables(&self) -> Result<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_tasks'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_tasks USING vec0(task_id TEXT PRIMARY KEY, embedding float[384]);\n\
                 CREATE VIRTUAL TABLE vec_artifacts USING vec0(artifact_id TEXT PRIMARY KEY, embedding float[384]);",
            )?;
            tracing::info!("vector tables created");
        }
        Ok(())
    }
}

fn register_sqlite_vec() -> bool {
    type InitFn = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    unsafe {
        let init: InitFn = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(init));
        rc == rusqlite::ffi::SQLITE_OK
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
