-- Migration 026 (LM-10842): finish artifact→knowledge rename
--
-- Two cleanups land together because they share the same logical surface
-- (the artifact identifier on the envelope contract and the SQL alias view):
--
-- 1. Rewrite envelope predicate JSON inside `task_envelopes.json`
--      "type": "artifact_exists" → "type": "knowledge_exists"
--      "artifact_type":          → "knowledge_type":
--    These are the discriminator and the filter key of the `knowledge_exists`
--    precondition/postcondition. The evaluator (src/envelope/conditions.rs)
--    only recognises the new names after this migration; old DB rows that
--    still carry the legacy spelling would silently fail evaluation as an
--    "unknown predicate type", so they MUST be rewritten in place.
--
-- 2. Drop the backwards-compat `VIEW artifacts → knowledge` (introduced by
--    024, rebuilt by 025). All daemon code now reads `knowledge` directly
--    and the v3.0 baseline declares the alias dead. Triggers that bounce
--    writes through the view are dropped first because SQLite refuses to
--    DROP a view while triggers reference it.
--
-- Idempotency: REPLACE() is a no-op when the substring is absent; the WHERE
-- clause keeps the UPDATE from rewriting unrelated rows. DROP IF EXISTS is
-- safe on a re-run.
--
-- Envelope JSON is stored verbatim (not canonicalised), so submitters could
-- have used either compact or whitespace-padded forms. We cover both common
-- shapes; pathological spacing inside a key/value pair would need json_set
-- gymnastics and is not produced by any current daemon/CLI/web path.

UPDATE task_envelopes
SET json = REPLACE(
             REPLACE(
               REPLACE(
                 REPLACE(json,
                   '"type":"artifact_exists"', '"type":"knowledge_exists"'),
                 '"type": "artifact_exists"', '"type": "knowledge_exists"'),
               '"artifact_type":', '"knowledge_type":'),
             '"artifact_type" :', '"knowledge_type" :')
WHERE json LIKE '%artifact_exists%' OR json LIKE '%artifact_type%';

DROP TRIGGER IF EXISTS artifacts_alias_no_insert;
DROP TRIGGER IF EXISTS artifacts_alias_no_update;
DROP TRIGGER IF EXISTS artifacts_alias_no_delete;
DROP VIEW IF EXISTS artifacts;

INSERT OR IGNORE INTO schema_version (version, applied_at)
VALUES (26, strftime('%s','now') * 1000);
