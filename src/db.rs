use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// FIX-DAEMON-103 T3: r2d2 pool type aliases. The pool replaces the legacy
/// `Mutex<Connection>` model — every handler acquires its own owned connection
/// and returns it on drop. SQLite WAL mode (set during migrations and persisted
/// in the file header) gives multi-reader concurrency for free; writers serialize
/// via SQLite's own busy timeout.
pub type SqlitePool = r2d2::Pool<SqliteConnectionManager>;
pub type SqlitePooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// Maximum schema version this binary supports. If the on-disk DB reports a
/// higher version, the daemon refuses to start (downgrade guard).
pub const SCHEMA_VERSION_MAX: i64 = 26;

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "001_initial.sql", include_str!("../migrations/001_initial.sql")),
    (2, "002_envelope.sql", include_str!("../migrations/002_envelope.sql")),
    (
        3,
        "003_task_usage.sql",
        include_str!("../migrations/003_task_usage.sql"),
    ),
    (
        4,
        "004_task_locks.sql",
        include_str!("../migrations/004_task_locks.sql"),
    ),
    (
        5,
        "005_activity_log_retention.sql",
        include_str!("../migrations/005_activity_log_retention.sql"),
    ),
    (
        6,
        "006_envelope_fts.sql",
        include_str!("../migrations/006_envelope_fts.sql"),
    ),
    (
        7,
        "007_run_envelope_snapshot.sql",
        include_str!("../migrations/007_run_envelope_snapshot.sql"),
    ),
    (
        8,
        "008_atomic_size_hint.sql",
        include_str!("../migrations/008_atomic_size_hint.sql"),
    ),
    (
        11,
        "011_tier_column.sql",
        include_str!("../migrations/011_tier_column.sql"),
    ),
    (
        12,
        "012_cycles_unit_id.sql",
        include_str!("../migrations/012_cycles_unit_id.sql"),
    ),
    (
        13,
        "013_drop_unit_status_approval.sql",
        include_str!("../migrations/013_drop_unit_status_approval.sql"),
    ),
    (
        14,
        "014_qa_fields.sql",
        include_str!("../migrations/014_qa_fields.sql"),
    ),
    (
        15,
        "015_wiki_tree.sql",
        include_str!("../migrations/015_wiki_tree.sql"),
    ),
    (
        16,
        "016_audit_log.sql",
        include_str!("../migrations/016_audit_log.sql"),
    ),
    (
        17,
        "017_artifact_scope_reclassify.sql",
        include_str!("../migrations/017_artifact_scope_reclassify.sql"),
    ),
    (
        18,
        "018_audit_log_prev_hash.sql",
        include_str!("../migrations/018_audit_log_prev_hash.sql"),
    ),
    (
        19,
        "019_project_cwd_unique.sql",
        include_str!("../migrations/019_project_cwd_unique.sql"),
    ),
    (
        20,
        "020_tier_default_med.sql",
        include_str!("../migrations/020_tier_default_med.sql"),
    ),
    (
        21,
        "021_tier_used.sql",
        include_str!("../migrations/021_tier_used.sql"),
    ),
    (
        22,
        "022_evidence_batch.sql",
        include_str!("../migrations/022_evidence_batch.sql"),
    ),
    (
        23,
        "023_scenario_id_index_alias.sql",
        include_str!("../migrations/023_scenario_id_index_alias.sql"),
    ),
    (
        24,
        "024_artifacts_to_knowledge.sql",
        include_str!("../migrations/024_artifacts_to_knowledge.sql"),
    ),
    (
        25,
        "025_drop_scope_from_compat_view.sql",
        include_str!("../migrations/025_drop_scope_from_compat_view.sql"),
    ),
    (
        26,
        "026_envelope_predicate_to_knowledge.sql",
        include_str!("../migrations/026_envelope_predicate_to_knowledge.sql"),
    ),
];

pub struct Db {
    pub conn: Connection,
    pub vec_enabled: bool,
    /// US-CKT-SCHEMA-038: Optional state directory for migration.log. When None,
    /// migration events still flow to the tracing subscriber but no dedicated
    /// log file is appended. Production daemons set this from `Paths::state`
    /// (typically ~/.local/state/clawket/).
    pub state_dir: Option<PathBuf>,
    /// FIX-DAEMON-103 T3: original DB path retained so `into_pool()` can build
    /// a `SqliteConnectionManager` against the same file after the migration
    /// connection is dropped.
    db_path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_state(path, None)
    }

    /// US-CKT-SCHEMA-038: open the DB and route migration events to a dedicated
    /// `migration.log` file under `state_dir` in addition to the tracing
    /// subscriber. The log appends one line per (start, success, failure) event
    /// with millisecond timestamps so post-incident debugging can reconstruct
    /// the exact sequence without scraping stderr.
    pub fn open_with_state(path: &Path, state_dir: Option<PathBuf>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir: {}", parent.display()))?;
        }

        let vec_enabled = register_sqlite_vec();

        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // FIX-DAEMON-110: SQLite BUSY retry — wait up to 5 s before giving up.
        // This lets concurrent writers queue behind the lock instead of immediately
        // returning SQLITE_BUSY → 503. The error.rs From<rusqlite::Error> handler
        // maps any residual BUSY (after timeout) to HTTP 503 with code "SQLITE_BUSY".
        conn.pragma_update(None, "busy_timeout", 5000)?;

        let mut db = Self {
            conn,
            vec_enabled,
            state_dir,
            db_path: path.to_path_buf(),
        };
        // FIX-DAEMON-110: backup DB before running migrations (idempotent: skip if already at max)
        backup_if_needed(path, &db)?;
        db.migrate()?;
        if db.vec_enabled {
            db.ensure_vector_tables()?;
        }
        Ok(db)
    }

    /// FIX-DAEMON-103 T3: consume the migrated `Db` and produce a connection
    /// pool against the same file. The single migration connection is dropped
    /// first so the pool starts with a clean slate.
    ///
    /// Per-connection pragmas (foreign_keys, busy_timeout) are applied on each
    /// new pool connection via `SqliteConnectionManager::with_init`. WAL mode
    /// is a database-file-level setting persisted in the SQLite header, so it
    /// is inherited automatically. sqlite-vec is registered globally via
    /// `sqlite3_auto_extension` (in `register_sqlite_vec`), so every future
    /// connection — including pool members — auto-loads it.
    ///
    /// Pool sizing: max 16 connections matches the daemon's tokio worker pool
    /// (typically 8 cores × 2). Connection timeout 30 s — exceeding it means a
    /// real concurrency runaway, not a transient blip; we want a loud panic
    /// instead of a silent indefinite hang.
    pub fn into_pool(self) -> SqlitePool {
        let path = self.db_path.clone();
        // Drop the migration conn explicitly before opening the pool. This
        // releases its file handle so pool conns don't briefly contend with
        // a half-idle migration conn.
        drop(self.conn);

        let manager = SqliteConnectionManager::file(&path).with_init(
            |conn: &mut Connection| -> rusqlite::Result<()> {
                // Per-connection pragmas. WAL is file-level and already set
                // during migrations; foreign_keys + busy_timeout are not.
                conn.pragma_update(None, "foreign_keys", "ON")?;
                conn.pragma_update(None, "busy_timeout", 5000)?;
                Ok(())
            },
        );

        r2d2::Pool::builder()
            .max_size(16)
            .connection_timeout(std::time::Duration::from_secs(30))
            .build(manager)
            .expect("build sqlite r2d2 pool")
    }

    /// Return the highest schema version recorded in the DB (0 if none yet).
    pub fn current_schema_version(&self) -> i64 {
        let has_table: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_table {
            return 0;
        }
        self.conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))
            .unwrap_or(0)
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

        // FIX-DAEMON-021: refuse to open a DB that was created by a newer binary.
        // A downgrade would run old code against an unknown schema; data corruption
        // or panics are near-certain. Exit code 4 = daemon config / path error.
        if current > SCHEMA_VERSION_MAX {
            anyhow::bail!(
                "SCHEMA_DOWNGRADE_REFUSED: on-disk schema version {} > binary max {}. \
                 Upgrade clawketd or point CLAWKET_DB at a compatible database.",
                current,
                SCHEMA_VERSION_MAX
            );
        }

        for (version, file, sql) in MIGRATIONS {
            if *version <= current {
                continue;
            }
            // US-CKT-SCHEMA-038: emit start event before opening the tx so the
            // log captures the attempt even if the SQL fails immediately.
            let start_ts = now_ms();
            log_migration_event(&self.state_dir, start_ts, *version, file, "start", None);
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
                    log_migration_event(
                        &self.state_dir,
                        now_ms(),
                        *version,
                        file,
                        "success",
                        None,
                    );
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
                        log_migration_event(
                            &self.state_dir,
                            now_ms(),
                            *version,
                            file,
                            "noop_duplicate",
                            None,
                        );
                    } else {
                        log_migration_event(
                            &self.state_dir,
                            now_ms(),
                            *version,
                            file,
                            "failure",
                            Some(&msg),
                        );
                        return Err(e).with_context(|| format!("migration {file}"));
                    }
                }
            }
        }

        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    fn ensure_vector_tables(&self) -> Result<()> {
        let has_tasks: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_tasks'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_tasks {
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_tasks USING vec0(task_id TEXT PRIMARY KEY, embedding float[384]);",
            )?;
            tracing::info!("vec_tasks created");
        }
        // LM-PLAN-01KR020034 / U2: vec_artifacts → vec_knowledge.
        // Legacy DBs (pre-migration-024) may carry `vec_artifacts`. The daemon
        // only writes/reads `vec_knowledge` from now on; legacy table is left
        // untouched (data is regenerated on demand and the virtual table has
        // no on-disk schema impact).
        let has_knowledge: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_knowledge'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_knowledge {
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_knowledge USING vec0(knowledge_id TEXT PRIMARY KEY, embedding float[384]);",
            )?;
            tracing::info!("vec_knowledge created");
        }
        Ok(())
    }
}

/// US-CKT-SCHEMA-038: append a single migration event to <state_dir>/migration.log.
///
/// Format: `<ts_ms>\t<version>\t<file>\t<status>[\terror=<msg>]\n`. Best-effort —
/// failure to open / append the file is logged via tracing but never aborts
/// the migration. The directory is created on demand.
fn log_migration_event(
    state_dir: &Option<PathBuf>,
    ts_ms: i64,
    version: i64,
    file: &str,
    status: &str,
    error: Option<&str>,
) {
    let Some(dir) = state_dir else { return };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "create state dir for migration.log failed");
        return;
    }
    let log_path = dir.join("migration.log");
    let line = match error {
        Some(msg) => format!("{ts_ms}\t{version}\t{file}\t{status}\terror={msg}\n"),
        None => format!("{ts_ms}\t{version}\t{file}\t{status}\n"),
    };
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!(error = %e, path = %log_path.display(), "append migration.log failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %log_path.display(), "open migration.log failed");
        }
    }
}

/// FIX-DAEMON-110: backup DB before running migrations.
/// Creates a `.bak-vN` copy only when migrations are needed (current < MAX).
/// Skip if DB does not exist yet (first run), or if already at MAX (no migrations to run).
/// The backup path is `<db>.bak-v<current>` e.g. `db.sqlite.bak-v16`.
/// On failure (disk full, permissions) we log a warning but do NOT abort — the
/// backup is best-effort; we must not block daemon start over it.
fn backup_if_needed(db_path: &Path, db: &Db) -> Result<()> {
    let current = db.current_schema_version();
    // No backup needed if already at max (no migrations will run)
    if current >= SCHEMA_VERSION_MAX {
        return Ok(());
    }
    // Skip if DB file doesn't exist (in-memory or new DB)
    if !db_path.exists() {
        return Ok(());
    }
    let backup_path = db_path.with_extension(format!("sqlite.bak-v{current}"));
    if backup_path.exists() {
        // Already have a backup for this version — skip
        return Ok(());
    }
    match std::fs::copy(db_path, &backup_path) {
        Ok(_) => {
            tracing::info!(
                from = %db_path.display(),
                to = %backup_path.display(),
                schema_version = current,
                "pre-migration backup created"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %backup_path.display(),
                "pre-migration backup FAILED (continuing anyway — disk full?)"
            );
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open_only_migration_1(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute_batch(MIGRATIONS[0].2).unwrap();
        // Migration 001 records its own schema_version row, so no manual insert.
        conn
    }

    #[test]
    fn migration_002_applies_on_v1_db_and_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upgrade.db");

        // Stand up a v1 schema with realistic prior data.
        {
            let conn = open_only_migration_1(&path);
            conn.execute(
                "INSERT INTO projects (id, name, created_at, updated_at)
                 VALUES ('PROJ-X', 'X', ?1, ?1)",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plans (id, project_id, title, source, created_at, status)
                 VALUES ('PLAN-X', 'PROJ-X', 'P1', 'manual', ?1, 'active')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO units (id, plan_id, idx, title, execution_mode, approval_required, created_at, status)
                 VALUES ('UNIT-X', 'PLAN-X', 0, 'U1', 'sequential', 0, ?1, 'todo')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, unit_id, idx, title, body, priority, type, created_at, status)
                 VALUES ('TASK-X', 'UNIT-X', 0, 'T1', '', 'normal', 'feature', ?1, 'todo')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO artifacts (id, task_id, type, title, content, content_format, scope, created_at)
                 VALUES ('ART-X', 'TASK-X', 'note', 'a', 'body', 'markdown', 'rag', ?1)",
                rusqlite::params![now_ms()],
            )
            .unwrap();
        }

        // Re-open via Db::open which should auto-apply migration 002.
        let db = Db::open(&path).unwrap();

        // Migrations recorded — schema head moves with new files. Migration
        // versions are NOT necessarily contiguous (9, 10 were retired), so
        // compare MAX(version) against the last MIGRATIONS entry's version
        // rather than against MIGRATIONS.len().
        let max: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        let expected_head = MIGRATIONS.last().unwrap().0;
        assert_eq!(max, expected_head);

        // task_envelopes table exists.
        let has_env: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_envelopes'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_env, "task_envelopes table missing after migration");

        // tasks.active_envelope_id column exists.
        let has_col: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'active_envelope_id'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_col, "tasks.active_envelope_id column missing");

        // runs.envelope_id column exists.
        let has_runs_col: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('runs') WHERE name = 'envelope_id'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_runs_col, "runs.envelope_id column missing");

        // Pre-existing data preserved.
        let task_id: String = db
            .conn
            .query_row("SELECT id FROM tasks WHERE id = 'TASK-X'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(task_id, "TASK-X");
        let knowledge_id: String = db
            .conn
            .query_row(
                "SELECT id FROM knowledge WHERE id = 'ART-X'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(knowledge_id, "ART-X");

        // active_envelope_id is NULL on legacy task (per ADR-0011 forward-migration policy).
        let active: Option<String> = db
            .conn
            .query_row(
                "SELECT active_envelope_id FROM tasks WHERE id = 'TASK-X'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(active.is_none(), "legacy task should have NULL active_envelope_id");
    }

    #[test]
    fn migration_002_idempotent_on_already_v2_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idem.db");

        // Open twice — second open must be a no-op.
        Db::open(&path).unwrap();
        let db = Db::open(&path).unwrap();
        let max: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        let expected_head = MIGRATIONS.last().unwrap().0;
        assert_eq!(max, expected_head);
    }

    // ---- LM-255 / L1.3.a ----

    #[test]
    fn migration_008_atomic_size_hint_columns_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic_size_hint.db");
        let db = Db::open(&path).unwrap();

        let has_hint: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'atomic_size_hint'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_hint, "tasks.atomic_size_hint column missing");

        let has_policy: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'decomposition_policy'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_policy, "tasks.decomposition_policy column missing");
    }

    #[test]
    fn migration_008_atomic_size_hint_backfills_legacy_rows() {
        // Legacy DB stood up at v7, then upgraded — pre-existing tasks must
        // surface as 'small' / 'auto' without manual data fill.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backfill.db");

        // Apply migrations 1..=7 manually so we can insert a "legacy" task row.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            for (_, _, sql) in &MIGRATIONS[..7] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute(
                "INSERT INTO projects (id, name, created_at, updated_at)
                 VALUES ('PROJ-LEGACY', 'L', ?1, ?1)",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plans (id, project_id, title, source, created_at, status)
                 VALUES ('PLAN-L', 'PROJ-LEGACY', 'P', 'manual', ?1, 'active')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO units (id, plan_id, idx, title, execution_mode, approval_required, created_at, status)
                 VALUES ('UNIT-L', 'PLAN-L', 0, 'U', 'sequential', 0, ?1, 'todo')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, unit_id, idx, title, body, priority, type, created_at, status)
                 VALUES ('TASK-LEGACY', 'UNIT-L', 0, 'T', '', 'normal', 'feature', ?1, 'todo')",
                rusqlite::params![now_ms()],
            )
            .unwrap();
        }

        // Re-open via Db::open → migration 008 runs.
        let db = Db::open(&path).unwrap();
        let (hint, policy): (String, String) = db
            .conn
            .query_row(
                "SELECT atomic_size_hint, decomposition_policy
                 FROM tasks WHERE id = 'TASK-LEGACY'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(hint, "small");
        assert_eq!(policy, "auto");
    }

    /// US-CKT-SCHEMA-050 (R4 redefined): daemon-source-only round-trip test for
    /// migrations 022 + 023. Stand up a v2 schema (versions 1..=21, before
    /// evidence/batch_id are added), insert legacy task rows, then reopen via
    /// `Db::open` so migrations 022 + 023 run automatically. Verify:
    ///   (a) `tasks` has the three v3 columns: scenario_id, evidence, batch_id
    ///   (b) legacy rows have NULL in all three v3 columns (forward-migration policy)
    ///   (c) original v2 task fields (id, title, body, status, created_at) survive
    ///
    /// Scope: daemon repo only — no cross-repo or live-DB dependencies. PDD O8
    /// keeps this test in-tempfile; it never touches ~/.local/share/clawket.
    #[test]
    fn migration_022_023_v3_columns_preserve_v2_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2_to_v3_roundtrip.db");

        // Apply migrations 1..=21 manually so we land on a true v2 schema:
        // scenario_id already exists (added in 014), but evidence + batch_id do not.
        // MIGRATIONS layout: indices 0..=18 cover versions 1..=21, index 19 = v22, index 20 = v23.
        let v2_slice_end = MIGRATIONS
            .iter()
            .position(|(v, _, _)| *v == 22)
            .expect("migration 022 must exist");
        assert_eq!(v2_slice_end, 19, "MIGRATIONS layout drift — update slice");

        let created_at = now_ms();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            for (_, _, sql) in &MIGRATIONS[..v2_slice_end] {
                conn.execute_batch(sql).unwrap();
            }

            // Sanity: v2 schema has scenario_id but NOT evidence / batch_id.
            let has_scenario: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'scenario_id'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(has_scenario, "v2 schema should already have scenario_id (migration 014)");
            let has_evidence: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'evidence'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(!has_evidence, "v2 schema must NOT yet have evidence (added in 022)");
            let has_batch: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'batch_id'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(!has_batch, "v2 schema must NOT yet have batch_id (added in 022)");

            // Insert v2-shaped legacy data. Note: tasks columns at v21 include
            // scenario_id (NULL) but no evidence/batch_id yet.
            conn.execute(
                "INSERT INTO projects (id, name, created_at, updated_at)
                 VALUES ('PROJ-V2', 'V2', ?1, ?1)",
                rusqlite::params![created_at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plans (id, project_id, title, source, created_at, status)
                 VALUES ('PLAN-V2', 'PROJ-V2', 'V2 plan', 'manual', ?1, 'active')",
                rusqlite::params![created_at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO units (id, plan_id, idx, title, created_at, execution_mode)
                 VALUES ('UNIT-V2', 'PLAN-V2', 0, 'V2 unit', ?1, 'sequential')",
                rusqlite::params![created_at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, unit_id, idx, title, body, priority, type, created_at, status)
                 VALUES ('TASK-V2-A', 'UNIT-V2', 0, 'legacy A', 'body A', 'normal', 'feature', ?1, 'todo')",
                rusqlite::params![created_at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tasks (id, unit_id, idx, title, body, priority, type, created_at, status)
                 VALUES ('TASK-V2-B', 'UNIT-V2', 1, 'legacy B', 'body B', 'high', 'feature', ?1, 'in_progress')",
                rusqlite::params![created_at],
            )
            .unwrap();
        }

        // Reopen via Db::open → migrations 022 + 023 run automatically.
        let db = Db::open(&path).unwrap();

        // Migration head should now be at SCHEMA_VERSION_MAX after Db::open.
        let max: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(max, SCHEMA_VERSION_MAX, "schema head must reach SCHEMA_VERSION_MAX after Db::open");

        // (a) The three v3 columns now exist on tasks.
        for col in &["scenario_id", "evidence", "batch_id"] {
            let exists: bool = db
                .conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tasks') WHERE name = ?1",
                    rusqlite::params![col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(exists, "tasks.{col} column missing after v2→v3 migration");
        }

        // Migration 023 also adds the alias index `idx_tasks_scenario_id`.
        let has_alias_idx: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_tasks_scenario_id'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_alias_idx, "migration 023 alias index idx_tasks_scenario_id missing");

        // (b) Legacy rows must surface with NULL in all three v3 columns.
        for tid in &["TASK-V2-A", "TASK-V2-B"] {
            let (sid, evi, bid): (Option<String>, Option<String>, Option<String>) = db
                .conn
                .query_row(
                    "SELECT scenario_id, evidence, batch_id FROM tasks WHERE id = ?1",
                    rusqlite::params![tid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert!(sid.is_none(), "{tid}.scenario_id should be NULL on legacy row");
            assert!(evi.is_none(), "{tid}.evidence should be NULL on legacy row");
            assert!(bid.is_none(), "{tid}.batch_id should be NULL on legacy row");
        }

        // (c) Original v2 fields preserved verbatim.
        let (id_a, title_a, body_a, status_a, created_a): (String, String, String, String, i64) = db
            .conn
            .query_row(
                "SELECT id, title, body, status, created_at FROM tasks WHERE id = 'TASK-V2-A'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(id_a, "TASK-V2-A");
        assert_eq!(title_a, "legacy A");
        assert_eq!(body_a, "body A");
        assert_eq!(status_a, "todo");
        assert_eq!(created_a, created_at);

        let (id_b, title_b, body_b, status_b, created_b): (String, String, String, String, i64) = db
            .conn
            .query_row(
                "SELECT id, title, body, status, created_at FROM tasks WHERE id = 'TASK-V2-B'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(id_b, "TASK-V2-B");
        assert_eq!(title_b, "legacy B");
        assert_eq!(body_b, "body B");
        assert_eq!(status_b, "in_progress");
        assert_eq!(created_b, created_at);
    }

    #[test]
    fn migration_008_atomic_size_hint_rejects_invalid_enum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enum.db");
        let db = Db::open(&path).unwrap();

        // CHECK constraint must reject a value outside the enum.
        let err = db
            .conn
            .execute(
                "INSERT INTO projects (id, name, created_at, updated_at)
                 VALUES ('P', 'p', 0, 0)",
                [],
            )
            .and_then(|_| {
                db.conn.execute(
                    "INSERT INTO plans (id, project_id, title, source, created_at, status)
                     VALUES ('PL', 'P', 't', 'manual', 0, 'active')",
                    [],
                )
            })
            .and_then(|_| {
                db.conn.execute(
                    "INSERT INTO units (id, plan_id, idx, title, execution_mode, approval_required, created_at, status)
                     VALUES ('U', 'PL', 0, 'u', 'sequential', 0, 0, 'todo')",
                    [],
                )
            })
            .and_then(|_| {
                db.conn.execute(
                    "INSERT INTO tasks (id, unit_id, idx, title, body, priority, type, created_at, status, atomic_size_hint)
                     VALUES ('T', 'U', 0, 't', '', 'normal', 'feature', 0, 'todo', 'gigantic')",
                    [],
                )
            });
        assert!(err.is_err(), "atomic_size_hint='gigantic' should fail CHECK");
    }
}
