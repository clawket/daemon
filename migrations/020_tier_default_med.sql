-- FIX-DAEMON-r2-tier: Tier column DEFAULT='med' (v3 plan).
-- Round-2 QA finding: TIER-002/003/005/007/009/010/020/040..050.
-- Migration 011 set DEFAULT NULL; v3 spec mandates DEFAULT 'med' so a task
-- created without an explicit --tier still has a queryable tier value.
--
-- SQLite cannot ALTER COLUMN ... SET DEFAULT in-place; we have to rebuild
-- the table to change the DEFAULT clause. To minimise disruption we instead
-- (a) backfill NULL → 'med' so existing rows have med, and (b) rely on the
-- application layer (repo/tasks::create) to fall back to 'med' when input.tier
-- is None.
-- The CHECK constraint from 011 is preserved since we are not rebuilding.

UPDATE tasks SET tier = 'med' WHERE tier IS NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (20, strftime('%s','now') * 1000);
