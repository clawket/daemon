//! Task execution locks (RL-U3-14 / LM-67).
//!
//! `acquire` succeeds when the row is missing, expired, or already owned by
//! the requesting session (in which case it just refreshes the TTL — handy
//! for crash-resume in the same session). It returns `Conflict` with the
//! existing live lock when a different session holds it.
//!
//! `release` and `heartbeat` are session-scoped: they no-op + return None
//! when the lock has already been reclaimed by someone else, so a slow
//! caller can't stomp on a fresh holder.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::id::now_ms;

#[derive(Debug, Clone, Serialize)]
pub struct TaskLock {
    pub task_id: String,
    pub session_id: String,
    pub acquired_at: i64,
    pub expires_at: i64,
    pub heartbeat_at: i64,
}

pub enum AcquireOutcome {
    /// Lock granted (newly created, expired-and-reclaimed, or refreshed by
    /// the same session).
    Acquired(TaskLock),
    /// Held by a different live session.
    Conflict(TaskLock),
}

fn row_to_lock(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskLock> {
    Ok(TaskLock {
        task_id: row.get(0)?,
        session_id: row.get(1)?,
        acquired_at: row.get(2)?,
        expires_at: row.get(3)?,
        heartbeat_at: row.get(4)?,
    })
}

/// Read the current row regardless of expiry.
pub fn get(conn: &Connection, task_id: &str) -> Result<Option<TaskLock>> {
    let row = conn
        .query_row(
            "SELECT task_id, session_id, acquired_at, expires_at, heartbeat_at
             FROM task_locks WHERE task_id = ?1",
            params![task_id],
            row_to_lock,
        )
        .optional()?;
    Ok(row)
}

/// Try to take the lock for `session_id` for `ttl_ms` milliseconds.
///
/// Same-session reacquire refreshes the TTL. Cross-session contention with a
/// live lock returns `Conflict` carrying the holder. Expired rows are
/// reclaimed in place.
pub fn acquire(
    conn: &Connection,
    task_id: &str,
    session_id: &str,
    ttl_ms: i64,
) -> Result<AcquireOutcome> {
    let now = now_ms();
    let existing = get(conn, task_id)?;
    let stale = match &existing {
        Some(l) => l.expires_at <= now || l.session_id == session_id,
        None => true,
    };

    if !stale {
        // SAFETY: stale==false ⇒ existing is Some.
        return Ok(AcquireOutcome::Conflict(existing.unwrap()));
    }

    let acquired_at = match &existing {
        Some(l) if l.session_id == session_id && l.expires_at > now => l.acquired_at,
        _ => now,
    };
    let expires_at = now + ttl_ms;
    conn.execute(
        "INSERT INTO task_locks (task_id, session_id, acquired_at, expires_at, heartbeat_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(task_id) DO UPDATE SET
             session_id   = excluded.session_id,
             acquired_at  = excluded.acquired_at,
             expires_at   = excluded.expires_at,
             heartbeat_at = excluded.heartbeat_at",
        params![task_id, session_id, acquired_at, expires_at, now],
    )?;
    Ok(AcquireOutcome::Acquired(TaskLock {
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        acquired_at,
        expires_at,
        heartbeat_at: now,
    }))
}

/// Extend the TTL of a lock the caller already holds. Returns `None` if the
/// lock has been reclaimed by someone else (or never existed).
pub fn heartbeat(
    conn: &Connection,
    task_id: &str,
    session_id: &str,
    ttl_ms: i64,
) -> Result<Option<TaskLock>> {
    let now = now_ms();
    let expires_at = now + ttl_ms;
    let updated = conn.execute(
        "UPDATE task_locks
         SET expires_at = ?1, heartbeat_at = ?2
         WHERE task_id = ?3 AND session_id = ?4 AND expires_at > ?2",
        params![expires_at, now, task_id, session_id],
    )?;
    if updated == 0 {
        return Ok(None);
    }
    get(conn, task_id)
}

/// Release a lock owned by `session_id`. Returns `true` if a row was
/// deleted, `false` otherwise (already released or owned by someone else).
pub fn release(conn: &Connection, task_id: &str, session_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM task_locks WHERE task_id = ?1 AND session_id = ?2",
        params![task_id, session_id],
    )?;
    Ok(n > 0)
}

/// Forcefully reclaim the lock regardless of session — for admin / doctor
/// use only. Production wiring lands alongside `clawket doctor` (LM-69+).
#[allow(dead_code)]
pub fn force_release(conn: &Connection, task_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM task_locks WHERE task_id = ?1",
        params![task_id],
    )?;
    Ok(n > 0)
}

impl std::fmt::Debug for AcquireOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireOutcome::Acquired(l) => write!(f, "Acquired(session={})", l.session_id),
            AcquireOutcome::Conflict(l) => write!(f, "Conflict(session={})", l.session_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, tasks, units};

    fn open_db_with_task() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("t.db")).unwrap();
        let p = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: None,
                key: None,
            },
        )
        .unwrap()
        .unwrap();
        let pl = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &p.id,
                title: "v1",
                description: None,
                source: None,
                source_path: None,
                auto_advance: false,
            },
        )
        .unwrap()
        .unwrap();
        plans::approve(&db.conn, &pl.id).unwrap();
        let u = units::create(
            &db.conn,
            units::CreateInput {
                plan_id: &pl.id,
                title: "U",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        let t = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id: &u.id,
                title: "T",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
                atomic_size_hint: None,
                decomposition_policy: None,
                tier: None,
                qa_status: None,
                scenario_id: None,
                defect_task: None,
                scenario_amendment: None,
                evidence: None,
                batch_id: None,
            },
        )
        .unwrap()
        .unwrap();
        (dir, db, t.id)
    }

    #[test]
    fn acquire_creates_new_lock_when_none_exists() {
        let (_dir, db, task_id) = open_db_with_task();
        let res = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        match res {
            AcquireOutcome::Acquired(l) => {
                assert_eq!(l.task_id, task_id);
                assert_eq!(l.session_id, "session-A");
                assert!(l.expires_at > l.acquired_at);
            }
            other => panic!("expected Acquired, got {:?}", other),
        }
    }

    #[test]
    fn acquire_blocks_other_session_with_live_lock() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        let res = acquire(&db.conn, &task_id, "session-B", 60_000).unwrap();
        match res {
            AcquireOutcome::Conflict(holder) => assert_eq!(holder.session_id, "session-A"),
            other => panic!("expected Conflict, got {:?}", other),
        }
    }

    #[test]
    fn acquire_same_session_refreshes_ttl() {
        let (_dir, db, task_id) = open_db_with_task();
        let first = match acquire(&db.conn, &task_id, "session-A", 60_000).unwrap() {
            AcquireOutcome::Acquired(l) => l,
            _ => unreachable!(),
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = match acquire(&db.conn, &task_id, "session-A", 120_000).unwrap() {
            AcquireOutcome::Acquired(l) => l,
            other => panic!("expected Acquired, got {:?}", other),
        };
        assert_eq!(second.acquired_at, first.acquired_at);
        assert!(second.expires_at >= first.expires_at);
    }

    #[test]
    fn acquire_reclaims_expired_lock_for_new_session() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        // Force expire by rewriting both timestamps to the past — the CHECK
        // constraint enforces expires_at >= acquired_at, so we move them
        // together.
        db.conn
            .execute(
                "UPDATE task_locks SET acquired_at = 1, expires_at = 1, heartbeat_at = 1
                 WHERE task_id = ?1",
                params![&task_id],
            )
            .unwrap();
        let res = acquire(&db.conn, &task_id, "session-B", 60_000).unwrap();
        match res {
            AcquireOutcome::Acquired(l) => assert_eq!(l.session_id, "session-B"),
            other => panic!("expected Acquired (reclaim), got {:?}", other),
        }
    }

    #[test]
    fn heartbeat_extends_ttl_for_owner() {
        let (_dir, db, task_id) = open_db_with_task();
        let first = match acquire(&db.conn, &task_id, "session-A", 30_000).unwrap() {
            AcquireOutcome::Acquired(l) => l,
            _ => unreachable!(),
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        let extended = heartbeat(&db.conn, &task_id, "session-A", 120_000)
            .unwrap()
            .expect("heartbeat should succeed");
        assert!(extended.expires_at > first.expires_at);
        assert!(extended.heartbeat_at >= first.heartbeat_at);
    }

    #[test]
    fn heartbeat_returns_none_for_non_owner() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        let res = heartbeat(&db.conn, &task_id, "session-B", 60_000).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn heartbeat_returns_none_when_lock_already_expired() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        db.conn
            .execute(
                "UPDATE task_locks SET acquired_at = 1, expires_at = 1, heartbeat_at = 1
                 WHERE task_id = ?1",
                params![&task_id],
            )
            .unwrap();
        let res = heartbeat(&db.conn, &task_id, "session-A", 60_000).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn release_only_succeeds_for_owner() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        assert!(!release(&db.conn, &task_id, "session-B").unwrap());
        assert!(release(&db.conn, &task_id, "session-A").unwrap());
        assert!(get(&db.conn, &task_id).unwrap().is_none());
    }

    #[test]
    fn force_release_drops_any_owner() {
        let (_dir, db, task_id) = open_db_with_task();
        let _ = acquire(&db.conn, &task_id, "session-A", 60_000).unwrap();
        assert!(force_release(&db.conn, &task_id).unwrap());
        assert!(get(&db.conn, &task_id).unwrap().is_none());
    }
}
