-- FIX-DAEMON-013: ISO 8601 timestamps (stored as TEXT in new columns)
-- FIX-DAEMON-014: Rename activity_log → audit_log + new columns
-- Scenarios: U13 TEL-011/013/014, U14 TIER-007/035, U11 I18N-070..074
--
-- NOTE: Existing timestamp columns (created_at, etc.) remain as INTEGER epoch-ms
-- for backward compat. New audit_log table uses TEXT ISO 8601 timestamps.
-- The Rust layer serializes epoch-ms to ISO 8601 strings in JSON responses (FIX-013).

-- Create audit_log as replacement for activity_log
CREATE TABLE IF NOT EXISTS audit_log (
  id          TEXT PRIMARY KEY,
  entity_type TEXT NOT NULL,
  entity_id   TEXT NOT NULL,
  op_type     TEXT NOT NULL CHECK (op_type IN (
                 'status_change', 'created', 'deleted', 'updated',
                 'approved', 'activated', 'completed', 'POLICY_VIOLATION'
               )),
  field       TEXT,
  old_value   TEXT,
  new_value   TEXT,
  actor       TEXT NOT NULL DEFAULT 'cli' CHECK (actor IN ('claude', 'cli', 'external-api', 'system')),
  at          TEXT NOT NULL  -- ISO 8601 UTC, e.g. 2026-05-04T12:00:00.000Z
);
CREATE INDEX IF NOT EXISTS idx_audit_log_entity ON audit_log(entity_type, entity_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_time   ON audit_log(at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor  ON audit_log(actor);
CREATE INDEX IF NOT EXISTS idx_audit_log_op     ON audit_log(op_type);

-- Migrate existing activity_log rows into audit_log
-- Map old 'action' → op_type (best-effort), epoch ms → ISO 8601 string
INSERT OR IGNORE INTO audit_log (id, entity_type, entity_id, op_type, field, old_value, new_value, actor, at)
SELECT
  id,
  entity_type,
  entity_id,
  CASE action
    WHEN 'status_change' THEN 'status_change'
    WHEN 'created'       THEN 'created'
    WHEN 'deleted'       THEN 'deleted'
    WHEN 'updated'       THEN 'updated'
    ELSE                      'updated'
  END,
  field,
  old_value,
  new_value,
  CASE
    WHEN actor IS NULL OR actor = '' OR actor = 'system' THEN 'cli'
    WHEN actor = 'human' THEN 'cli'
    WHEN actor LIKE 'claude%' OR actor = 'main' THEN 'claude'
    ELSE 'cli'
  END,
  -- Convert epoch ms to ISO 8601
  strftime('%Y-%m-%dT%H:%M:%f', created_at / 1000.0, 'unixepoch') || 'Z'
FROM activity_log;

-- Keep activity_log for backward compatibility (activity_log_rollup job still references it)
-- Rename would break activity_log_archive which is referenced in jobs.
-- We keep both tables during this migration round. activity_log remains for
-- the rollup job; audit_log is the new canonical table.

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (16, strftime('%s','now') * 1000);
