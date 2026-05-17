-- Migration 002: Execution envelope (ADR-0001)
-- Adds task_envelopes sidecar + active_envelope_id pointer on tasks.
--
-- Forward-migration safe: existing tasks get NULL active_envelope_id.
-- Dashboard surfaces them as "legacy (no contract)".
-- Per ADR-0011, version=1 envelopes only at M0; v2+ adds to this schema.

-- ============================================================
-- task_envelopes (the 19-field execution envelope, sidecar)
-- ============================================================
CREATE TABLE IF NOT EXISTS task_envelopes (
  id            TEXT PRIMARY KEY,                                 -- ENV-<ulid>
  task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  version       INTEGER NOT NULL,
  json          TEXT NOT NULL,                                    -- 19-field document, validated by daemon at insert
  signed_at     INTEGER NOT NULL,
  signed_by     TEXT NOT NULL,                                    -- author or agent_id
  superseded_by TEXT REFERENCES task_envelopes(id),
  CHECK (json_valid(json)),
  CHECK (version >= 1),
  UNIQUE (task_id, version)
);

CREATE INDEX IF NOT EXISTS idx_envelopes_task   ON task_envelopes(task_id);
CREATE INDEX IF NOT EXISTS idx_envelopes_active ON task_envelopes(task_id) WHERE superseded_by IS NULL;
CREATE INDEX IF NOT EXISTS idx_envelopes_signed ON task_envelopes(signed_at);

-- ============================================================
-- tasks.active_envelope_id pointer
-- ============================================================
ALTER TABLE tasks ADD COLUMN active_envelope_id TEXT REFERENCES task_envelopes(id);
CREATE INDEX IF NOT EXISTS idx_tasks_active_envelope ON tasks(active_envelope_id);

-- ============================================================
-- runs.envelope_id (capture which envelope the run was against)
-- ============================================================
-- runs table is created in migration 001; the column may already exist on dev
-- machines that ran an in-progress migration. Use ADD COLUMN; the migrate()
-- runner has a "duplicate column" path that no-ops cleanly (db.rs:73).
ALTER TABLE runs ADD COLUMN envelope_id TEXT REFERENCES task_envelopes(id);
CREATE INDEX IF NOT EXISTS idx_runs_envelope ON runs(envelope_id);
