-- FIX-DAEMON-111: add UNIQUE constraint on project_cwds.cwd so a working
-- directory can belong to at most one project.
--
-- SQLite does not support ALTER TABLE ADD CONSTRAINT. We must recreate the
-- table with the new constraint and migrate data.
--
-- De-duplicate first: if the same cwd somehow ended up under multiple projects
-- (bug in old code) keep only the oldest (smallest rowid) entry.

CREATE TABLE IF NOT EXISTS project_cwds_new (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  cwd        TEXT NOT NULL,
  PRIMARY KEY (project_id, cwd),
  UNIQUE (cwd)
);

-- Copy data, skipping duplicates (keep first registered owner per cwd)
INSERT OR IGNORE INTO project_cwds_new (project_id, cwd)
SELECT project_id, cwd FROM project_cwds
ORDER BY rowid ASC;

DROP TABLE project_cwds;
ALTER TABLE project_cwds_new RENAME TO project_cwds;

CREATE INDEX IF NOT EXISTS idx_project_cwds_cwd ON project_cwds(cwd);

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (19, strftime('%s','now') * 1000);
