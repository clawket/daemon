-- US-CKT-SCHEMA-011/016/019/021-026: evidence + batch_id columns on tasks.
-- evidence  : free-text evidence string (file:line or reasoning summary), max 4 KiB enforced at API layer.
-- batch_id  : ULID identifying the sub-agent batch invocation that produced the task status.
--
-- Additive-only (ALTER TABLE ADD COLUMN). DROP/DELETE/TRUNCATE are forbidden (PDD O8).

ALTER TABLE tasks ADD COLUMN evidence TEXT DEFAULT NULL;
ALTER TABLE tasks ADD COLUMN batch_id TEXT DEFAULT NULL;

CREATE INDEX IF NOT EXISTS idx_tasks_batch_id ON tasks(batch_id) WHERE batch_id IS NOT NULL;
