-- US-CKT-SCHEMA-040: scenario contract specifies the index name
-- `idx_tasks_scenario_id` (with `_id` suffix), but migration 014 created
-- `idx_tasks_scenario` (no suffix). Per PDD O8 we cannot DROP the existing
-- index, so we add an alias index with the contract-specified name. SQLite
-- will choose either index for `WHERE scenario_id = ?` lookups; query plans
-- remain unchanged.
--
-- This is purely additive — both indexes coexist. Disk overhead is negligible
-- since the partial-index WHERE clause limits coverage to non-NULL rows only.

CREATE INDEX IF NOT EXISTS idx_tasks_scenario_id ON tasks(scenario_id) WHERE scenario_id IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (23, strftime('%s','now') * 1000);
