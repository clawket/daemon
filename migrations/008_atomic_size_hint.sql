-- Migration 008 — tasks.atomic_size_hint + tasks.decomposition_policy (LM-255 / L1.3.a).
--
-- These ADR-0001 envelope fields previously lived only inside the JSON blob in
-- task_envelopes.json. That meant `task decompose` had to parse the active
-- envelope per-task to know when to enforce a split — slow at fan-out time and
-- impossible to filter on in SQL. Promoting both to first-class columns lets
-- import/export hit a single canonical surface and lets the strict parser
-- (LM-263) populate them without touching the envelope sidecar.
--
-- atomic_size_hint enum mirrors the strict-format spec
-- (cli/docs/plans/strict-format.md): tiny|small|medium|large. Default 'small'
-- backfills legacy rows without flagging them as oversized — `task decompose`
-- only triggers on medium/large.
--
-- decomposition_policy is intentionally free-form (no CHECK) to match the
-- envelope's policy expression grammar (e.g. `tree(max_depth=3)`,
-- `atomic`, `auto`). 'auto' is the safe default — daemon-side scheduler
-- picks the policy from atomic_size_hint when the field reads 'auto'.

ALTER TABLE tasks ADD COLUMN atomic_size_hint TEXT NOT NULL DEFAULT 'small'
  CHECK (atomic_size_hint IN ('tiny', 'small', 'medium', 'large'));

ALTER TABLE tasks ADD COLUMN decomposition_policy TEXT NOT NULL DEFAULT 'auto';

CREATE INDEX IF NOT EXISTS idx_tasks_atomic_size_hint ON tasks(atomic_size_hint);
