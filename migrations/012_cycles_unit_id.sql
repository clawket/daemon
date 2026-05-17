-- FIX-DAEMON-002: cycles.unit_id NOT NULL + FK to units(id)
-- Scenarios: U10 PDD-030/003/031/032
-- SQLite does not support ADD COLUMN ... NOT NULL without a default,
-- so we add with DEFAULT then validate/backfill existing NULL rows to
-- a synthetic "Default Unit" approach. The daemon will create a default
-- unit per project if needed at migration time. Since ALTER TABLE ADD COLUMN
-- with a FK reference is also limited in SQLite, we use a two-step approach:
-- 1. Add nullable column with FK-style comment (FK enforced via app logic and trigger)
-- 2. Daemon startup code (db.rs) backfills NULL rows to a default unit and
--    marks the column NOT NULL via table rebuild (see db.rs ensure_unit_id_not_null).

-- Step 1: Add the column (nullable first, app enforces NOT NULL on new writes)
ALTER TABLE cycles ADD COLUMN unit_id TEXT REFERENCES units(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_cycles_unit ON cycles(unit_id);

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (12, strftime('%s','now') * 1000);
