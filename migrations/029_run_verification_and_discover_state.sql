-- Migration 029: evidence-by-reference + discover-loop durable state.
--
-- 1. runs verification record. A task's evidence may cite `run:<RUN-ID>`
--    instead of free text; the citation is only accepted when the run recorded
--    the command it executed, a digest of its inputs, its exit code and a
--    digest of the report it produced (repo::runs::validate_evidence_refs).
--    Nullable: runs that are not verification runs keep working unchanged.
ALTER TABLE runs ADD COLUMN command TEXT;
ALTER TABLE runs ADD COLUMN input_hash TEXT;
ALTER TABLE runs ADD COLUMN exit_code INTEGER;
ALTER TABLE runs ADD COLUMN report_hash TEXT;

-- 2. harness_error verdict. A verification row whose harness failed (the check
--    could not run) is not a defect and must not move the defect tally, but it
--    also means the scenario has no result, so the round cannot converge.
--    tasks.qa_status carries a CHECK constraint from migration 014 whose value
--    set cannot be widened without rebuilding `tasks` (FTS content rowid,
--    triggers); the harness failure is therefore recorded in its own column.
--    Non-NULL = the latest verification attempt for this task failed in the
--    harness; any later pass/defect/scenario_error transcription clears it.
ALTER TABLE tasks ADD COLUMN harness_error TEXT;

-- 3. Retry queue keyed by (scenario, failure signature). `attempts` is the
--    source of truth for the retry cap enforced at sync time.
CREATE TABLE IF NOT EXISTS discover_retry_queue (
  project_id  TEXT NOT NULL,
  scenario_id TEXT NOT NULL,
  signature   TEXT NOT NULL,
  attempts    INTEGER NOT NULL DEFAULT 0,
  last_status TEXT NOT NULL,
  last_error  TEXT,
  exhausted   INTEGER NOT NULL DEFAULT 0,
  first_seen  INTEGER NOT NULL,
  last_seen   INTEGER NOT NULL,
  PRIMARY KEY (project_id, scenario_id, signature)
);

-- 4. Per-round snapshot keyed by plan id + domain key (not by plan title).
CREATE TABLE IF NOT EXISTS discover_round_snapshots (
  project_id     TEXT NOT NULL,
  domain_key     TEXT NOT NULL,
  round          INTEGER NOT NULL,
  plan_id        TEXT NOT NULL,
  pass           INTEGER NOT NULL,
  defect         INTEGER NOT NULL,
  scenario_error INTEGER NOT NULL,
  harness_error  INTEGER NOT NULL,
  total          INTEGER NOT NULL,
  decision       TEXT NOT NULL,
  recorded_at    INTEGER NOT NULL,
  PRIMARY KEY (project_id, domain_key, round)
);
CREATE INDEX IF NOT EXISTS idx_discover_round_snapshots_plan
  ON discover_round_snapshots(plan_id);
