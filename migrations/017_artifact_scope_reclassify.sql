-- FIX-DAEMON-103: Artifact scope reclassification migration
-- v3 removes scope=reference|archive on write.  Existing rows with those
-- scopes are migrated to rag (reference → rag, archive stays for soft-deleted).
--
-- reference → rag: these were meant to be searchable long-term content.
-- archive remains archive (soft-deleted wiki artifacts).

UPDATE artifacts
SET scope = 'rag'
WHERE scope = 'reference';

-- Add a CHECK constraint to prevent future invalid scope values.
-- SQLite does not support ADD CONSTRAINT on existing tables, so we only
-- document the valid set here; the application layer enforces it via
-- validate_scope_for_write() in repo/artifacts.rs.
--
-- Valid scopes: rag | working | archive
-- rag     = searchable, returned by /artifacts?scope=rag (default)
-- working = draft / ephemeral, not indexed
-- archive = soft-deleted, excluded from list by default

INSERT OR IGNORE INTO schema_version (version, applied_at)
VALUES (17, strftime('%s','now') * 1000);
