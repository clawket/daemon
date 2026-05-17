-- Migration 007 — runs.status + runs.envelope_snapshot (RL-U4-08b / LM-176).
--
-- Why a frozen JSON snapshot instead of relying on envelope_id (added in 002):
--   envelope_id points to the active envelope row at run-create time, but the
--   envelope sidecar is append-only — newer signatures supersede older ones.
--   For audit, a run must reproduce *exactly* what was sent to the agent, even
--   if the live envelope drifted afterward (parent re-signed, decomposition
--   policy edited). We therefore freeze the resolved envelope as a JSON blob
--   at the moment POST /runs is accepted. envelope_id stays for joinability;
--   envelope_snapshot stays for reproducibility.
--
-- status='pending' lets execute_task hand back a prompt without immediately
-- claiming the task as started — the agent flips it to 'started' once the
-- LLM call begins. This decouples envelope freezing from execution clock.

ALTER TABLE runs ADD COLUMN status TEXT NOT NULL DEFAULT 'started'
  CHECK (status IN ('pending', 'started', 'finished', 'aborted'));

ALTER TABLE runs ADD COLUMN envelope_snapshot TEXT;

CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
