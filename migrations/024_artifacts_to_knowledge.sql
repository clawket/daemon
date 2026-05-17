-- LM-PLAN-01KR020034 / U1: artifacts → knowledge rename + scope column drop
-- Decision: LM-10807 (option C — archive rows pre-exported to wiki paths by U7,
-- then deleted before this migration runs)
--
-- Pre-condition (enforced by U7 export task, not by this SQL):
--   1. archive scope rows (66건) exported to lattice-mono/docs/qa-archive/
--      with file count == 66 verified.
--   2. archive rows DELETED FROM artifacts table.
--      => After U7: SELECT COUNT(*) FROM artifacts WHERE scope='archive' == 0.
--
-- This migration is non-destructive for surviving rows (rag + working).
-- Reference scope is already collapsed to rag by 017_artifact_scope_reclassify.sql,
-- so post-017 valid scopes are only {rag, working, archive}; after U7 only
-- {rag, working} remain.
--
-- Strategy: SQLite has no DROP COLUMN before 3.35; even with newer SQLite the
-- artifacts_fts virtual table binds content='artifacts' and several triggers
-- reference the artifacts name. Cleanest approach: rebuild table.
--
-- Step plan:
--   1. Drop FTS triggers + FTS virtual table backed by artifacts.
--   2. CREATE TABLE knowledge (new schema, no scope column).
--   3. INSERT ... SELECT to copy rag+working rows from artifacts.
--   4. Recreate artifact_versions FK to point to knowledge.
--   5. CREATE artifacts_fts equivalent as knowledge_fts on knowledge.
--   6. Recreate triggers (insert/delete/update + depth_check) on knowledge.
--   7. Drop old artifacts table.
--   8. Create backwards-compat VIEW artifacts -> knowledge (for U2/U3/U4 alias
--      grace period). VIEW is read-only; daemon must route writes to knowledge.
--   9. Bump schema_version to 24.
--
-- IMPORTANT: rusqlite migration runner executes this inside a transaction.
-- BEGIN/COMMIT not needed.

-- ============================================================================
-- 1. Drop FTS sync infrastructure tied to old name
-- ============================================================================
DROP TRIGGER IF EXISTS artifacts_ai;
DROP TRIGGER IF EXISTS artifacts_ad;
DROP TRIGGER IF EXISTS artifacts_au;
DROP TRIGGER IF EXISTS artifacts_depth_check;
DROP TABLE IF EXISTS artifacts_fts;

-- ============================================================================
-- 2. Knowledge table (no scope column)
-- ============================================================================
CREATE TABLE knowledge (
  id             TEXT PRIMARY KEY,                           -- KN-<ulid> (legacy ART- accepted via alias)
  task_id        TEXT REFERENCES tasks(id) ON DELETE CASCADE,
  unit_id        TEXT REFERENCES units(id) ON DELETE CASCADE,
  plan_id        TEXT REFERENCES plans(id) ON DELETE CASCADE,
  type           TEXT NOT NULL,                              -- decision|design|wireframe|adr|note|link|spec
  title          TEXT NOT NULL,
  content        TEXT NOT NULL DEFAULT '',
  content_format TEXT NOT NULL DEFAULT 'md',                 -- md|json|yaml
  parent_id      TEXT REFERENCES knowledge(id),
  wiki_idx       INTEGER DEFAULT 0,
  wiki_depth     INTEGER DEFAULT 0,
  created_at     INTEGER NOT NULL,
  CHECK (
    (task_id IS NOT NULL) OR
    (unit_id IS NOT NULL) OR
    (plan_id IS NOT NULL)
  )
);

-- ============================================================================
-- 3. Copy surviving rows (rag + working). Archive rows must be 0 by U7.
-- Guard: assert archive count is zero before copy.
-- ============================================================================
-- Defensive guard: if archive rows still exist, fail loudly rather than silently
-- discarding them. Implementation note: rusqlite executes statements
-- sequentially; we use a SELECT that raises via abs(json_error) trick? SQLite
-- has no native ASSERT. Instead we emulate with a SELECT that fails on
-- constraint violation when archive count > 0.
--
-- Approach: temporary trigger-less constraint via INSERT into a 0-row CHECK
-- helper.
CREATE TEMP TABLE _migrate_024_guard (
  archive_count INTEGER NOT NULL CHECK (archive_count = 0)
);
INSERT INTO _migrate_024_guard
  SELECT COUNT(*) FROM artifacts WHERE scope = 'archive';
DROP TABLE _migrate_024_guard;

INSERT INTO knowledge (
  id, task_id, unit_id, plan_id, type, title, content, content_format,
  parent_id, wiki_idx, wiki_depth, created_at
)
SELECT
  id, task_id, unit_id, plan_id, type, title, content, content_format,
  parent_id, wiki_idx, wiki_depth, created_at
FROM artifacts
WHERE scope IN ('rag', 'working');

-- ============================================================================
-- 4. Migrate artifact_versions FK to knowledge
-- artifact_versions.artifact_id REFERENCES artifacts(id) currently; rename
-- the table and rebuild FK.
-- ============================================================================
CREATE TABLE knowledge_versions (
  id             TEXT PRIMARY KEY,
  knowledge_id   TEXT NOT NULL REFERENCES knowledge(id) ON DELETE CASCADE,
  version        INTEGER NOT NULL,
  content        TEXT,
  content_format TEXT,
  created_at     INTEGER,
  created_by     TEXT
);

INSERT INTO knowledge_versions (
  id, knowledge_id, version, content, content_format, created_at, created_by
)
SELECT
  av.id, av.artifact_id, av.version, av.content, av.content_format,
  av.created_at, av.created_by
FROM artifact_versions av
WHERE av.artifact_id IN (SELECT id FROM knowledge);

DROP TABLE artifact_versions;

-- ============================================================================
-- 5. Indexes (mirror old artifacts indexes minus scope index)
-- ============================================================================
CREATE INDEX IF NOT EXISTS idx_knowledge_task     ON knowledge(task_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_unit     ON knowledge(unit_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_plan     ON knowledge(plan_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_type     ON knowledge(type);
CREATE INDEX IF NOT EXISTS idx_knowledge_parent   ON knowledge(parent_id) WHERE parent_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_knowledge_wiki_idx ON knowledge(parent_id, wiki_idx);
CREATE INDEX IF NOT EXISTS idx_knowledge_versions_knowledge ON knowledge_versions(knowledge_id);

-- ============================================================================
-- 6. FTS virtual table on knowledge
-- ============================================================================
CREATE VIRTUAL TABLE knowledge_fts USING fts5(
  title, content,
  content='knowledge',
  content_rowid='rowid',
  tokenize='unicode61'
);

INSERT INTO knowledge_fts(rowid, title, content)
  SELECT rowid, title, content FROM knowledge;

CREATE TRIGGER knowledge_ai AFTER INSERT ON knowledge BEGIN
  INSERT INTO knowledge_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;
CREATE TRIGGER knowledge_ad AFTER DELETE ON knowledge BEGIN
  INSERT INTO knowledge_fts(knowledge_fts, rowid, title, content) VALUES('delete', old.rowid, old.title, old.content);
END;
CREATE TRIGGER knowledge_au AFTER UPDATE ON knowledge BEGIN
  INSERT INTO knowledge_fts(knowledge_fts, rowid, title, content) VALUES('delete', old.rowid, old.title, old.content);
  INSERT INTO knowledge_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;

-- ============================================================================
-- 7. Depth check trigger (mirror 015_wiki_tree.sql semantics on knowledge)
-- ============================================================================
CREATE TRIGGER knowledge_depth_check BEFORE INSERT ON knowledge
WHEN NEW.parent_id IS NOT NULL
BEGIN
  SELECT CASE
    WHEN (
      WITH RECURSIVE depth_walk(pid, d) AS (
        SELECT NEW.parent_id, 1
        UNION ALL
        SELECT k.parent_id, d + 1
        FROM knowledge k
        JOIN depth_walk dw ON k.id = dw.pid
        WHERE k.parent_id IS NOT NULL AND d < 10
      )
      SELECT COALESCE(MAX(d), 0) FROM depth_walk
    ) >= 8
    THEN RAISE(ABORT, 'WIKI_DEPTH_EXCEEDED: max depth is 8')
  END;
  SELECT CASE
    WHEN NEW.id = NEW.parent_id
    THEN RAISE(ABORT, 'SELF_PARENT: knowledge cannot be its own parent')
  END;
END;

-- ============================================================================
-- 8. Drop old artifacts table
-- ============================================================================
DROP TABLE artifacts;

-- ============================================================================
-- 9. Backwards-compat alias view (read-only)
-- Until U2/U3/U4/U5 land, external consumers may still query `artifacts`.
-- This view exposes knowledge rows with a synthetic scope='rag' column so
-- legacy queries keep working. Writes via this view are explicitly blocked
-- by INSTEAD OF triggers that raise an alias-deprecated error.
-- ============================================================================
CREATE VIEW artifacts AS
  SELECT
    id, task_id, unit_id, plan_id, type, title, content, content_format,
    parent_id, 'rag' AS scope, wiki_idx, wiki_depth, created_at
  FROM knowledge;

CREATE TRIGGER artifacts_alias_no_insert INSTEAD OF INSERT ON artifacts
BEGIN
  SELECT RAISE(ABORT, 'ARTIFACTS_ALIAS_DEPRECATED: write to knowledge directly');
END;

CREATE TRIGGER artifacts_alias_no_update INSTEAD OF UPDATE ON artifacts
BEGIN
  SELECT RAISE(ABORT, 'ARTIFACTS_ALIAS_DEPRECATED: write to knowledge directly');
END;

CREATE TRIGGER artifacts_alias_no_delete INSTEAD OF DELETE ON artifacts
BEGIN
  SELECT RAISE(ABORT, 'ARTIFACTS_ALIAS_DEPRECATED: delete from knowledge directly');
END;

-- ============================================================================
-- 10. Bump schema version
-- ============================================================================
INSERT OR IGNORE INTO schema_version (version, applied_at)
VALUES (24, strftime('%s','now') * 1000);
