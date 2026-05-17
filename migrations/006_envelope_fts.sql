-- Migration 006 — FTS5 indexing of envelope textual fields (RL-U3-05, LM-140).
--
-- Why a separate FTS table instead of extending tasks_fts:
--   tasks_fts uses content='tasks' (content-rowid follows tasks.rowid). Stuffing
--   envelope columns into the same virtual table would force a tasks UPDATE on
--   every envelope sign — wrong layer of coupling. The envelope table already
--   has its own append-only history; we mirror it 1:1 into a contentless FTS5
--   table and JOIN on rowid at query time. Active-envelope filter is applied in
--   the SELECT, not the index.

CREATE VIRTUAL TABLE IF NOT EXISTS task_envelopes_fts USING fts5(
  intent,
  prompt_template,
  success_criteria,
  content='',
  tokenize='unicode61'
);

-- INSERT trigger: index every signed envelope. json_extract returns NULL for
-- missing keys; FTS5 treats NULL as empty string, so legacy envelopes that
-- omit any of these fields are still safely indexed (they just don't hit on
-- those terms).
CREATE TRIGGER IF NOT EXISTS task_envelopes_fts_ai AFTER INSERT ON task_envelopes BEGIN
  INSERT INTO task_envelopes_fts(rowid, intent, prompt_template, success_criteria)
  VALUES (
    new.rowid,
    COALESCE(json_extract(new.json, '$.intent'), ''),
    COALESCE(json_extract(new.json, '$.prompt_template'), ''),
    COALESCE(json_extract(new.json, '$.success_criteria'), '')
  );
END;

-- DELETE trigger: drop the FTS row so cleared envelopes don't leak.
CREATE TRIGGER IF NOT EXISTS task_envelopes_fts_ad AFTER DELETE ON task_envelopes BEGIN
  INSERT INTO task_envelopes_fts(task_envelopes_fts, rowid, intent, prompt_template, success_criteria)
  VALUES (
    'delete',
    old.rowid,
    COALESCE(json_extract(old.json, '$.intent'), ''),
    COALESCE(json_extract(old.json, '$.prompt_template'), ''),
    COALESCE(json_extract(old.json, '$.success_criteria'), '')
  );
END;

-- UPDATE trigger: an envelope's json itself is immutable today, but
-- superseded_by mutations don't need re-indexing (the active filter handles
-- that). We still install a defensive UPDATE trigger that re-indexes only
-- when the json column changes — this keeps the FTS in sync if a future
-- migration ever rewrites envelope text in place.
CREATE TRIGGER IF NOT EXISTS task_envelopes_fts_au AFTER UPDATE OF json ON task_envelopes BEGIN
  INSERT INTO task_envelopes_fts(task_envelopes_fts, rowid, intent, prompt_template, success_criteria)
  VALUES (
    'delete',
    old.rowid,
    COALESCE(json_extract(old.json, '$.intent'), ''),
    COALESCE(json_extract(old.json, '$.prompt_template'), ''),
    COALESCE(json_extract(old.json, '$.success_criteria'), '')
  );
  INSERT INTO task_envelopes_fts(rowid, intent, prompt_template, success_criteria)
  VALUES (
    new.rowid,
    COALESCE(json_extract(new.json, '$.intent'), ''),
    COALESCE(json_extract(new.json, '$.prompt_template'), ''),
    COALESCE(json_extract(new.json, '$.success_criteria'), '')
  );
END;

-- Backfill: index every existing envelope. INSERT OR IGNORE prevents
-- double-indexing if a partial migration left some rows already populated.
INSERT INTO task_envelopes_fts(rowid, intent, prompt_template, success_criteria)
SELECT
  rowid,
  COALESCE(json_extract(json, '$.intent'), ''),
  COALESCE(json_extract(json, '$.prompt_template'), ''),
  COALESCE(json_extract(json, '$.success_criteria'), '')
FROM task_envelopes
WHERE rowid NOT IN (SELECT rowid FROM task_envelopes_fts);
