-- Migration 003: Token usage + cost tracking (RL-U3-12 / LM-65, ADR-0007 enforcement)
--
-- Records per-run input/output/cache token counts and USD cost so the daemon can:
--   1. answer GET /tasks/:id/usage (per-run history + totals)
--   2. answer GET /plans/:id/usage (model-grouped aggregation)
--   3. preflight a PreToolUse hook against envelope.token_budget — over-budget
--      tasks return HTTP 429 from the preflight endpoint.
--
-- Forward-migration safe (ADR-0011): legacy tasks have no usage rows, which is
-- treated as zero spend. token_budget_snapshot captures the envelope's budget
-- at the time of record so retroactive budget edits don't rewrite history.

CREATE TABLE IF NOT EXISTS task_usage (
  run_id                 TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
  task_id                TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  model                  TEXT NOT NULL,                                  -- e.g. claude-opus-4-7
  input_tokens           INTEGER NOT NULL DEFAULT 0,
  output_tokens          INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_write_tokens     INTEGER NOT NULL DEFAULT 0,
  cost_usd               REAL NOT NULL DEFAULT 0.0,
  started_at             INTEGER NOT NULL,
  ended_at               INTEGER,
  token_budget_snapshot  TEXT,                                           -- JSON copy of envelope.token_budget at record time
  CHECK (input_tokens >= 0),
  CHECK (output_tokens >= 0),
  CHECK (cache_read_tokens >= 0),
  CHECK (cache_write_tokens >= 0),
  CHECK (cost_usd >= 0.0),
  CHECK (token_budget_snapshot IS NULL OR json_valid(token_budget_snapshot))
);

CREATE INDEX IF NOT EXISTS idx_task_usage_task    ON task_usage(task_id);
CREATE INDEX IF NOT EXISTS idx_task_usage_model   ON task_usage(model);
CREATE INDEX IF NOT EXISTS idx_task_usage_started ON task_usage(started_at);
