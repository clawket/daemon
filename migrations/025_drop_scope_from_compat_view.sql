-- Drop the synthetic `scope` column from the `artifacts` compat view.
-- The view was introduced by migration 024 as a backwards-compat alias for
-- the renamed `knowledge` table; it exposed a hard-coded `'rag' AS scope`
-- column for legacy readers. The daemon no longer carries a scope concept,
-- so the view is rebuilt without it. INSTEAD OF triggers that previously
-- blocked writes through the alias are recreated unchanged.

DROP TRIGGER IF EXISTS artifacts_alias_no_insert;
DROP TRIGGER IF EXISTS artifacts_alias_no_update;
DROP TRIGGER IF EXISTS artifacts_alias_no_delete;
DROP VIEW IF EXISTS artifacts;

CREATE VIEW artifacts AS
  SELECT
    id, task_id, unit_id, plan_id, type, title, content, content_format,
    parent_id, wiki_idx, wiki_depth, created_at
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

INSERT OR IGNORE INTO schema_version (version, applied_at)
VALUES (25, strftime('%s','now') * 1000);
