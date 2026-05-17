-- FIX-DAEMON-004: Drop unit.status / approval columns (units are pure grouping)
-- Scenarios: U10 PDD-020/022
-- SQLite does not support DROP COLUMN directly in older versions.
-- We rebuild the table without status/approval_required/approved_by/approved_at/started_at/completed_at.
-- These columns are removed because units are pure grouping entities (no lifecycle state).

-- Rebuild units table without lifecycle columns
CREATE TABLE IF NOT EXISTS units_v3 (
  id             TEXT PRIMARY KEY,
  plan_id        TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
  idx            INTEGER NOT NULL,
  title          TEXT NOT NULL,
  goal           TEXT,
  created_at     INTEGER NOT NULL,
  execution_mode TEXT NOT NULL DEFAULT 'sequential',
  UNIQUE (plan_id, idx)
);

INSERT INTO units_v3 (id, plan_id, idx, title, goal, created_at, execution_mode)
SELECT id, plan_id, idx, title, goal, created_at, execution_mode FROM units;

DROP TABLE units;
ALTER TABLE units_v3 RENAME TO units;

CREATE INDEX IF NOT EXISTS idx_units_plan ON units(plan_id);

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (13, strftime('%s','now') * 1000);
