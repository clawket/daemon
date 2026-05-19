//! activity_log retention job (ADR-0010 / RL-U3-16, LM-69).
//!
//! Three-tier retention:
//!
//! 1. **Hot** (≤ `hot_days`): rows live in `activity_log`, never archived.
//! 2. **Warm** (`hot_days` < age ≤ `total_days`): copied into
//!    `activity_log_archive` (gzip JSON, one row per UTC day) AND kept inline
//!    in `activity_log` for fast read.
//! 3. **Cold** (> `total_days`): inline copy in `activity_log` is deleted; the
//!    `activity_log_archive` blob remains.
//!
//! Size budget (`max_mb`) caps the SUM of inline + archive byte size. When the
//! cap is exceeded the job shrinks the cold cutoff inward in 30-day steps
//! until the projected size fits, never below `MIN_TOTAL_DAYS_UNDER_PRESSURE`.
//!
//! `archived_at` on `activity_log` is the rollup checkpoint. A row with
//! `archived_at IS NOT NULL` has been copied into the archive and is eligible
//! for cold prune. Re-running the job is idempotent: the date-bucketed insert
//! into `activity_log_archive` is skipped when the period already exists, and
//! the source rows that already have `archived_at` set are not re-copied.
//!
//! The rollup runs in **one transaction per UTC-day batch** so a crash in the
//! middle of a multi-day catch-up loses at most one day's progress, never a
//! row's archived state.

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use std::io::Write;

use crate::id::new_id;
use crate::models::ActivityLogEntry;

const MS_PER_DAY: i64 = 86_400_000;
const DEFAULT_HOT_DAYS: i64 = 90;
const DEFAULT_TOTAL_DAYS: i64 = 365;
const DEFAULT_MAX_MB: i64 = 500;
const MIN_TOTAL_DAYS_UNDER_PRESSURE: i64 = 30;
const PRESSURE_SHRINK_STEP_DAYS: i64 = 30;
const INCREMENTAL_VACUUM_PAGES: i64 = 1024;

/// Retention policy. Read once from env at job start.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub hot_days: i64,
    pub total_days: i64,
    pub max_mb: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            hot_days: DEFAULT_HOT_DAYS,
            total_days: DEFAULT_TOTAL_DAYS,
            max_mb: DEFAULT_MAX_MB,
        }
    }
}

impl RetentionPolicy {
    /// Read policy from env; missing or unparseable values fall back to default.
    pub fn from_env() -> Self {
        Self {
            hot_days: env_i64("CLAWKET_ACTIVITY_LOG_HOT_DAYS", DEFAULT_HOT_DAYS),
            total_days: env_i64("CLAWKET_ACTIVITY_LOG_TOTAL_DAYS", DEFAULT_TOTAL_DAYS),
            max_mb: env_i64("CLAWKET_ACTIVITY_LOG_MAX_MB", DEFAULT_MAX_MB),
        }
    }
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Rollup outcome. Useful for tests and `tracing::info!`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RollupReport {
    pub batches_archived: usize,
    pub rows_archived: usize,
    pub rows_cold_pruned: usize,
    pub effective_total_days: i64,
    pub size_bytes_after: i64,
    pub over_budget: bool,
}

/// Total bytes used by activity_log + activity_log_archive. Approximated via
/// row sums; close enough for budget decisions and avoids a full PRAGMA scan.
pub fn current_size_bytes(conn: &Connection) -> Result<i64> {
    // activity_log: byte-length sum across the columns that grow.
    let inline: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(
                length(id) + length(entity_type) + length(entity_id) + length(action)
                + COALESCE(length(field), 0)
                + COALESCE(length(old_value), 0)
                + COALESCE(length(new_value), 0)
                + COALESCE(length(actor), 0)
                + 16
             ), 0) FROM activity_log",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let archive: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(byte_size), 0) FROM activity_log_archive",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(inline + archive)
}

/// Run the rollup once. Safe to call repeatedly; idempotent per UTC day.
pub fn run_once(
    conn: &mut Connection,
    policy: &RetentionPolicy,
    now_ms: i64,
) -> Result<RollupReport> {
    let hot_cutoff = now_ms - policy.hot_days * MS_PER_DAY;

    let mut report = RollupReport::default();

    // Phase 1 — archive everything in [oldest, hot_cutoff) one UTC day at a time.
    loop {
        let day = match next_unarchived_day(conn, hot_cutoff)? {
            Some(d) => d,
            None => break,
        };
        let archived = archive_one_day(conn, day, hot_cutoff)?;
        if archived == 0 {
            // The day batch was empty after we already had archive coverage —
            // mark archived_at on the residual rows so the loop can advance.
            mark_archived_in_period(conn, day.start_ms, day.end_ms, now_ms)?;
            continue;
        }
        report.batches_archived += 1;
        report.rows_archived += archived;
    }

    // Phase 2 — cold-prune. Determine effective cold cutoff under budget pressure.
    let mut effective_total = policy.total_days;
    let max_bytes = policy.max_mb.saturating_mul(1024 * 1024);
    let mut size = current_size_bytes(conn)?;
    while size > max_bytes && effective_total > MIN_TOTAL_DAYS_UNDER_PRESSURE {
        effective_total =
            (effective_total - PRESSURE_SHRINK_STEP_DAYS).max(MIN_TOTAL_DAYS_UNDER_PRESSURE);
        let cold_cutoff = now_ms - effective_total * MS_PER_DAY;
        let pruned = cold_prune(conn, cold_cutoff)?;
        report.rows_cold_pruned += pruned;
        size = current_size_bytes(conn)?;
    }
    if size <= max_bytes {
        // Steady-state cold cutoff still applies even when we're under budget.
        let cold_cutoff = now_ms - effective_total * MS_PER_DAY;
        let pruned = cold_prune(conn, cold_cutoff)?;
        report.rows_cold_pruned += pruned;
        size = current_size_bytes(conn)?;
    }

    report.effective_total_days = effective_total;
    report.size_bytes_after = size;
    report.over_budget = size > max_bytes;

    // Reclaim pages freed by the deletes. Failure is non-fatal — page density
    // catches up on the next clean pass.
    let _ = conn.pragma_update(None, "incremental_vacuum", INCREMENTAL_VACUUM_PAGES);

    Ok(report)
}

/// UTC day window in milliseconds since epoch.
#[derive(Debug, Clone, Copy)]
struct UtcDay {
    start_ms: i64,
    end_ms: i64,
}

fn floor_to_utc_day(ms: i64) -> i64 {
    // Negative ms (pre-1970 timestamps) shouldn't appear here, but the floor
    // would round toward zero in Rust's signed `%`; clamp defensively.
    if ms < 0 {
        return 0;
    }
    (ms / MS_PER_DAY) * MS_PER_DAY
}

/// Find the oldest UTC day that contains an unarchived row older than
/// `hot_cutoff_ms`, or `None` if the rollup is fully caught up.
fn next_unarchived_day(conn: &Connection, hot_cutoff_ms: i64) -> Result<Option<UtcDay>> {
    let oldest: Option<i64> = conn
        .query_row(
            "SELECT MIN(created_at) FROM activity_log
             WHERE archived_at IS NULL AND created_at < ?1",
            params![hot_cutoff_ms],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(oldest.map(|ms| {
        let start = floor_to_utc_day(ms);
        UtcDay {
            start_ms: start,
            end_ms: start + MS_PER_DAY,
        }
    }))
}

/// Archive one UTC day's worth of rows. Returns the count of newly archived
/// rows (already-archived rows in the period are skipped).
fn archive_one_day(conn: &mut Connection, day: UtcDay, hot_cutoff_ms: i64) -> Result<usize> {
    let upper = day.end_ms.min(hot_cutoff_ms);
    if upper <= day.start_ms {
        return Ok(0);
    }

    let tx = conn.transaction()?;

    // Skip if this day is already archived. Idempotency guard for the case
    // where a previous run wrote the archive blob but failed to mark
    // archived_at on the source rows; we just mark them here.
    let archive_present: bool = tx
        .query_row(
            "SELECT 1 FROM activity_log_archive WHERE period_start = ?1",
            params![day.start_ms],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if archive_present {
        let updated = tx.execute(
            "UPDATE activity_log SET archived_at = ?3
             WHERE archived_at IS NULL
               AND created_at >= ?1 AND created_at < ?2",
            params![day.start_ms, upper, ts_now()],
        )?;
        tx.commit()?;
        return Ok(updated);
    }

    // Fetch the rows for this UTC day that are still inside the hot cutoff
    // (defensive — `next_unarchived_day` already filtered, but the day might
    // straddle the cutoff if hot_days was just lowered).
    let mut stmt = tx.prepare(
        "SELECT id, entity_type, entity_id, action, field, old_value, new_value, actor, created_at
         FROM activity_log
         WHERE archived_at IS NULL AND created_at >= ?1 AND created_at < ?2
         ORDER BY created_at ASC",
    )?;
    let rows: Vec<ActivityLogEntry> = stmt
        .query_map(params![day.start_ms, upper], |r| {
            Ok(ActivityLogEntry {
                id: r.get(0)?,
                entity_type: r.get(1)?,
                entity_id: r.get(2)?,
                action: r.get(3)?,
                field: r.get(4)?,
                old_value: r.get(5)?,
                new_value: r.get(6)?,
                actor: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    if rows.is_empty() {
        tx.commit()?;
        return Ok(0);
    }

    let row_count = rows.len();
    let json = serde_json::to_vec(&rows).context("serialize activity_log batch")?;
    let gzipped = gzip(&json).context("gzip activity_log batch")?;
    let byte_size = gzipped.len() as i64;

    tx.execute(
        "INSERT INTO activity_log_archive (id, period_start, period_end, row_count, byte_size, created_at, gzip_blob)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            new_id("ALA"),
            day.start_ms,
            day.end_ms,
            row_count as i64,
            byte_size,
            ts_now(),
            gzipped,
        ],
    )?;

    tx.execute(
        "UPDATE activity_log SET archived_at = ?3
         WHERE archived_at IS NULL AND created_at >= ?1 AND created_at < ?2",
        params![day.start_ms, upper, ts_now()],
    )?;

    tx.commit()?;
    Ok(row_count)
}

fn mark_archived_in_period(conn: &Connection, start_ms: i64, end_ms: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE activity_log SET archived_at = ?3
         WHERE archived_at IS NULL AND created_at >= ?1 AND created_at < ?2",
        params![start_ms, end_ms, now],
    )?;
    Ok(())
}

fn cold_prune(conn: &Connection, cold_cutoff_ms: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM activity_log
         WHERE archived_at IS NOT NULL AND created_at < ?1",
        params![cold_cutoff_ms],
    )?;
    Ok(n)
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

fn ts_now() -> i64 {
    crate::id::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // The env-var tests mutate process state; cargo test's default thread pool
    // would let them stomp on each other. This mutex serializes the two
    // policy_* tests without forcing the whole module to single-threaded mode.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn open_db() -> (TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollup.db");
        let db = Db::open(&path).unwrap();
        (dir, db)
    }

    fn insert_log(conn: &Connection, id: &str, created_at: i64) {
        conn.execute(
            "INSERT INTO activity_log
             (id, entity_type, entity_id, action, field, old_value, new_value, actor, created_at)
             VALUES (?1, 'task', 'TASK-X', 'updated', 'status', 'todo', 'in_progress', 'agent', ?2)",
            params![id, created_at],
        )
        .unwrap();
    }

    #[test]
    fn policy_reads_env_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAWKET_ACTIVITY_LOG_HOT_DAYS", "30");
        std::env::set_var("CLAWKET_ACTIVITY_LOG_TOTAL_DAYS", "120");
        std::env::set_var("CLAWKET_ACTIVITY_LOG_MAX_MB", "100");
        let p = RetentionPolicy::from_env();
        assert_eq!(p.hot_days, 30);
        assert_eq!(p.total_days, 120);
        assert_eq!(p.max_mb, 100);
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_HOT_DAYS");
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_TOTAL_DAYS");
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_MAX_MB");
    }

    #[test]
    fn policy_rejects_zero_and_negative() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("CLAWKET_ACTIVITY_LOG_HOT_DAYS", "0");
        std::env::set_var("CLAWKET_ACTIVITY_LOG_TOTAL_DAYS", "-5");
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_MAX_MB");
        let p = RetentionPolicy::from_env();
        assert_eq!(p.hot_days, DEFAULT_HOT_DAYS);
        assert_eq!(p.total_days, DEFAULT_TOTAL_DAYS);
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_HOT_DAYS");
        std::env::remove_var("CLAWKET_ACTIVITY_LOG_TOTAL_DAYS");
    }

    #[test]
    fn rollup_no_op_when_all_rows_are_hot() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        insert_log(&db.conn, "LOG-1", now - 5 * MS_PER_DAY);
        insert_log(&db.conn, "LOG-2", now - 10 * MS_PER_DAY);

        let report = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        assert_eq!(report.batches_archived, 0);
        assert_eq!(report.rows_archived, 0);
        assert_eq!(report.rows_cold_pruned, 0);
    }

    #[test]
    fn rollup_archives_rows_older_than_hot_cutoff() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        // 2 rows older than 90 days, on the same UTC day.
        let old1 = now - 100 * MS_PER_DAY;
        let old2 = old1 + 3_600_000;
        insert_log(&db.conn, "LOG-OLD-1", old1);
        insert_log(&db.conn, "LOG-OLD-2", old2);
        // 1 hot row.
        insert_log(&db.conn, "LOG-HOT", now - 10 * MS_PER_DAY);

        let report = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        assert_eq!(report.batches_archived, 1);
        assert_eq!(report.rows_archived, 2);
        assert_eq!(report.rows_cold_pruned, 0, "100d < 365d total cutoff");

        // Source rows still present (warm tier) but archived_at is set.
        let archived_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM activity_log WHERE archived_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived_count, 2);

        // Archive batch present.
        let blobs: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM activity_log_archive", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(blobs, 1);
    }

    #[test]
    fn rollup_is_idempotent() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        insert_log(&db.conn, "LOG-OLD-1", now - 100 * MS_PER_DAY);

        let r1 = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        let r2 = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();

        assert_eq!(r1.rows_archived, 1);
        assert_eq!(r2.rows_archived, 0, "second pass must not re-archive");

        let blobs: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM activity_log_archive", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(blobs, 1, "archive table must not gain duplicate periods");
    }

    #[test]
    fn rollup_cold_prunes_rows_past_total_cutoff() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        // 400d ago — past the 365d cold cutoff.
        insert_log(&db.conn, "LOG-COLD", now - 400 * MS_PER_DAY);

        let report = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        assert_eq!(report.rows_archived, 1, "must be archived first");
        assert_eq!(report.rows_cold_pruned, 1, "then deleted from inline");

        let inline: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM activity_log", [], |r| r.get(0))
            .unwrap();
        let archive: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM activity_log_archive", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(inline, 0, "cold row removed from inline");
        assert_eq!(archive, 1, "archive blob retained");
    }

    #[test]
    fn rollup_archives_each_day_in_its_own_batch() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        // Day -100 and day -101 — two separate UTC days.
        insert_log(&db.conn, "LOG-D-100", now - 100 * MS_PER_DAY);
        insert_log(&db.conn, "LOG-D-101", now - 101 * MS_PER_DAY);

        let report = run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        assert_eq!(report.batches_archived, 2);
        assert_eq!(report.rows_archived, 2);
    }

    #[test]
    fn rollup_shrinks_cold_cutoff_under_budget_pressure() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        // Insert one row at day -200 (past 90d hot but under 365d cold).
        insert_log(&db.conn, "LOG-WARM-200", now - 200 * MS_PER_DAY);

        let policy = RetentionPolicy {
            hot_days: 90,
            total_days: 365,
            // 0 MB budget forces aggressive cold-prune.
            max_mb: 0,
        };
        let report = run_once(&mut db.conn, &policy, now).unwrap();
        assert_eq!(report.rows_archived, 1);
        // Even at zero budget the row at -200d must be deleted because the
        // shrunk cutoff (eventually 30d) sweeps past it.
        assert_eq!(report.rows_cold_pruned, 1);
        assert!(
            report.effective_total_days <= 200,
            "cold cutoff must have shrunk past 200d"
        );
    }

    #[test]
    fn rollup_refuses_to_shrink_below_minimum_under_pressure() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        // Row at day -10 (still hot, won't be archived).
        insert_log(&db.conn, "LOG-HOT", now - 10 * MS_PER_DAY);

        let policy = RetentionPolicy {
            hot_days: 5,
            total_days: 365,
            // Aggressive budget pressure.
            max_mb: 0,
        };
        let report = run_once(&mut db.conn, &policy, now).unwrap();
        assert!(
            report.effective_total_days >= MIN_TOTAL_DAYS_UNDER_PRESSURE,
            "must not shrink below {MIN_TOTAL_DAYS_UNDER_PRESSURE}d"
        );
    }

    #[test]
    fn current_size_bytes_includes_archive_blob() {
        let (_g, mut db) = open_db();
        let now = 1_700_000_000_000i64;
        insert_log(&db.conn, "LOG-OLD", now - 100 * MS_PER_DAY);

        let before = current_size_bytes(&db.conn).unwrap();
        run_once(&mut db.conn, &RetentionPolicy::default(), now).unwrap();
        let after = current_size_bytes(&db.conn).unwrap();
        assert!(
            after > before,
            "archive blob bytes should add to the total: before={before}, after={after}"
        );
    }

    #[test]
    fn floor_to_utc_day_aligns_correctly() {
        // 2023-11-14 00:00:00 UTC = 1_700_000_000_000 ms? Let's just check alignment.
        let ms = 1_700_001_234_567i64;
        let floored = floor_to_utc_day(ms);
        assert_eq!(floored % MS_PER_DAY, 0);
        assert!(floored <= ms);
        assert!(ms - floored < MS_PER_DAY);
    }
}
