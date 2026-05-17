-- FIX-DAEMON-008: Wiki tree feature for artifacts (parent_id already exists, add idx/depth)
-- Scenarios: U04 WIKI-001..008
-- parent_id already exists in artifacts table (from 001_initial.sql)
-- Add: idx (ordering within parent), depth (cached depth for fast queries)

ALTER TABLE artifacts ADD COLUMN wiki_idx INTEGER DEFAULT 0;
ALTER TABLE artifacts ADD COLUMN wiki_depth INTEGER DEFAULT 0;

-- Trigger to prevent depth > 8 (max_depth enforcement at DB level)
CREATE TRIGGER IF NOT EXISTS artifacts_depth_check BEFORE INSERT ON artifacts
WHEN NEW.parent_id IS NOT NULL
BEGIN
  SELECT CASE
    WHEN (
      WITH RECURSIVE depth_walk(pid, d) AS (
        SELECT NEW.parent_id, 1
        UNION ALL
        SELECT a.parent_id, d + 1
        FROM artifacts a
        JOIN depth_walk dw ON a.id = dw.pid
        WHERE a.parent_id IS NOT NULL AND d < 10
      )
      SELECT COALESCE(MAX(d), 0) FROM depth_walk
    ) >= 8
    THEN RAISE(ABORT, 'WIKI_DEPTH_EXCEEDED: max depth is 8')
  END;
  SELECT CASE
    WHEN NEW.id = NEW.parent_id
    THEN RAISE(ABORT, 'SELF_PARENT: artifact cannot be its own parent')
  END;
END;

CREATE INDEX IF NOT EXISTS idx_artifacts_parent ON artifacts(parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_artifacts_wiki_idx ON artifacts(parent_id, wiki_idx);

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (15, strftime('%s','now') * 1000);
