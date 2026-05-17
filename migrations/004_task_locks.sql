-- Migration 004: Task execution locks (RL-U3-14 / LM-67)
--
-- Prevents two agent sessions from running the same task concurrently —
-- which would otherwise race on envelope mutations and planned_sha. The
-- lock is TTL-based: holders extend it via heartbeat, abandoned locks
-- auto-reclaim once `expires_at < now()`.
--
-- task_id is the primary key, so a task has at most one active lock at any
-- time. Stale rows are not deleted on read (they would race with a fresh
-- acquire); the acquire path overwrites a stale row in place.

CREATE TABLE IF NOT EXISTS task_locks (
  task_id      TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
  session_id   TEXT NOT NULL,
  acquired_at  INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  heartbeat_at INTEGER NOT NULL,
  CHECK (expires_at >= acquired_at),
  CHECK (heartbeat_at >= acquired_at)
);

CREATE INDEX IF NOT EXISTS idx_task_locks_session ON task_locks(session_id);
CREATE INDEX IF NOT EXISTS idx_task_locks_expires ON task_locks(expires_at);
