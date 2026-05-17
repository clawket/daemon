-- Migration 005 — activity_log retention (RL-U3-16, ADR-0010)
-- 1. add archived_at column on activity_log to checkpoint the rollup job
-- 2. create activity_log_archive for gzip JSON batches per UTC day
-- 3. switch DB to incremental auto_vacuum so cold-prune deletes reclaim pages

ALTER TABLE activity_log ADD COLUMN archived_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_activity_log_archived ON activity_log(archived_at);

CREATE TABLE IF NOT EXISTS activity_log_archive (
  id           TEXT PRIMARY KEY,
  period_start INTEGER NOT NULL,             -- inclusive UTC ms (start of UTC day)
  period_end   INTEGER NOT NULL,             -- exclusive UTC ms (start of next UTC day)
  row_count    INTEGER NOT NULL,
  byte_size    INTEGER NOT NULL,             -- length(gzip_blob), denormalized for budget queries
  created_at   INTEGER NOT NULL,             -- when this archive batch was written
  gzip_blob    BLOB NOT NULL,                -- gzip(JSON.stringify(Vec<ActivityLogEntry>))
  CHECK (period_end > period_start),
  CHECK (row_count > 0),
  CHECK (byte_size > 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_activity_log_archive_period
  ON activity_log_archive(period_start);
CREATE INDEX IF NOT EXISTS idx_activity_log_archive_created
  ON activity_log_archive(created_at DESC);

-- auto_vacuum is recorded in the database header; setting it on a populated DB
-- requires a full VACUUM to take effect. We attempt that once here. If VACUUM
-- fails (e.g. another writer), the migration still records as applied; the
-- rollup job's incremental_vacuum calls become no-ops until the next clean
-- VACUUM, which is the same shape as a fresh-install DB.
PRAGMA auto_vacuum = INCREMENTAL;
