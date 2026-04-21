-- Clawket initial schema (consolidated)
--
-- Design principles:
--   P5 Cache-first: append-only bodies, volatile fields (status, assignee) at tail.
--   P2 Structured: no LLM summaries. Deterministic queries via FTS5.
--   P4 Isolation: task_id is the pull unit for sub-agents.
--
-- Entity model:
--   Project → Plan → Unit → Task
--   Cycle (time-boxed sprint) groups Tasks across units.
--   Artifact attached to exactly one of plan/unit/task.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- ============================================================
-- schema_version: tracks applied migrations
-- ============================================================
CREATE TABLE IF NOT EXISTS schema_version (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL
);

-- ============================================================
-- projects
-- ============================================================
CREATE TABLE IF NOT EXISTS projects (
  id          TEXT PRIMARY KEY,                              -- PROJ-<slug>
  name        TEXT NOT NULL UNIQUE,
  description TEXT,
  key         TEXT,                                          -- short uppercase id, e.g. CK
  enabled     INTEGER NOT NULL DEFAULT 1,
  wiki_paths  TEXT NOT NULL DEFAULT '["docs"]',              -- JSON array
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_key ON projects(key) WHERE key IS NOT NULL;

CREATE TABLE IF NOT EXISTS project_cwds (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  cwd        TEXT NOT NULL,
  PRIMARY KEY (project_id, cwd)
);
CREATE INDEX IF NOT EXISTS idx_project_cwds_cwd ON project_cwds(cwd);

-- ============================================================
-- plans
-- ============================================================
CREATE TABLE IF NOT EXISTS plans (
  id          TEXT PRIMARY KEY,                              -- PLAN-<ulid>
  project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title       TEXT NOT NULL,
  description TEXT,
  source      TEXT NOT NULL,                                 -- plan-mode|manual|aidlc|import
  source_path TEXT,
  created_at  INTEGER NOT NULL,
  approved_at INTEGER,
  status      TEXT NOT NULL DEFAULT 'draft'
);
CREATE INDEX IF NOT EXISTS idx_plans_project ON plans(project_id);
CREATE INDEX IF NOT EXISTS idx_plans_status ON plans(status);

-- ============================================================
-- units (approval unit; AIDLC bolt maps to one unit)
-- ============================================================
CREATE TABLE IF NOT EXISTS units (
  id                 TEXT PRIMARY KEY,                       -- UNIT-<ulid>
  plan_id            TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
  idx                INTEGER NOT NULL,
  title              TEXT NOT NULL,
  goal               TEXT,
  created_at         INTEGER NOT NULL,
  started_at         INTEGER,
  completed_at       INTEGER,
  approval_required  INTEGER NOT NULL DEFAULT 0,
  approved_by        TEXT,
  approved_at        INTEGER,
  execution_mode     TEXT NOT NULL DEFAULT 'sequential',
  status             TEXT NOT NULL DEFAULT 'pending',
  UNIQUE (plan_id, idx)
);
CREATE INDEX IF NOT EXISTS idx_units_plan ON units(plan_id);

-- ============================================================
-- cycles (time-boxed execution container)
-- ============================================================
CREATE TABLE IF NOT EXISTS cycles (
  id         TEXT PRIMARY KEY,                               -- CYC-<ulid>
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title      TEXT NOT NULL,
  goal       TEXT,
  idx        INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  ended_at   INTEGER,
  status     TEXT NOT NULL DEFAULT 'planning'
);
CREATE INDEX IF NOT EXISTS idx_cycles_project ON cycles(project_id);
CREATE INDEX IF NOT EXISTS idx_cycles_status  ON cycles(status);

-- ============================================================
-- tasks (execution unit; the primary work item)
-- ============================================================
CREATE TABLE IF NOT EXISTS tasks (
  id              TEXT PRIMARY KEY,                          -- TASK-<ulid>
  unit_id         TEXT NOT NULL REFERENCES units(id) ON DELETE CASCADE,
  idx             INTEGER NOT NULL,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL DEFAULT '',                  -- append-only work order
  created_at      INTEGER NOT NULL,
  started_at      INTEGER,
  completed_at    INTEGER,
  ticket_number   TEXT,                                      -- e.g. CK-123
  parent_task_id  TEXT REFERENCES tasks(id),
  priority        TEXT DEFAULT 'medium',
  complexity      TEXT,
  estimated_edits INTEGER,
  cycle_id        TEXT REFERENCES cycles(id),
  reporter        TEXT,
  type            TEXT DEFAULT 'task',
  agent_id        TEXT,
  status          TEXT NOT NULL DEFAULT 'todo',              -- todo|in_progress|blocked|done|cancelled
  assignee        TEXT,
  UNIQUE (unit_id, idx)
);
CREATE INDEX IF NOT EXISTS idx_tasks_unit          ON tasks(unit_id);
CREATE INDEX IF NOT EXISTS idx_tasks_status        ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_cycle         ON tasks(cycle_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_ticket_number ON tasks(ticket_number) WHERE ticket_number IS NOT NULL;

-- Task dependency graph
CREATE TABLE IF NOT EXISTS task_depends_on (
  task_id            TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, depends_on_task_id),
  CHECK (task_id != depends_on_task_id)
);

-- Task comments
CREATE TABLE IF NOT EXISTS task_comments (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  author     TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_comments_task ON task_comments(task_id);

-- Task labels
CREATE TABLE IF NOT EXISTS task_labels (
  task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  label   TEXT NOT NULL,
  PRIMARY KEY (task_id, label)
);
CREATE INDEX IF NOT EXISTS idx_task_labels_label ON task_labels(label);

-- Task typed relations (depends_on|blocks|supersedes|relates_to|duplicates)
CREATE TABLE IF NOT EXISTS task_relations (
  id             TEXT PRIMARY KEY,
  source_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  target_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  relation_type  TEXT NOT NULL,
  created_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_relations_source ON task_relations(source_task_id);
CREATE INDEX IF NOT EXISTS idx_task_relations_target ON task_relations(target_task_id);
CREATE INDEX IF NOT EXISTS idx_task_relations_type   ON task_relations(relation_type);

-- ============================================================
-- artifacts (decisions, designs, wireframes, ADRs, notes, links)
-- ============================================================
CREATE TABLE IF NOT EXISTS artifacts (
  id             TEXT PRIMARY KEY,                           -- ART-<ulid>
  task_id        TEXT REFERENCES tasks(id) ON DELETE CASCADE,
  unit_id        TEXT REFERENCES units(id) ON DELETE CASCADE,
  plan_id        TEXT REFERENCES plans(id) ON DELETE CASCADE,
  type           TEXT NOT NULL,                              -- decision|design|wireframe|adr|note|link
  title          TEXT NOT NULL,
  content        TEXT NOT NULL DEFAULT '',
  content_format TEXT NOT NULL DEFAULT 'md',                 -- md|json|yaml
  parent_id      TEXT REFERENCES artifacts(id),
  scope          TEXT NOT NULL DEFAULT 'reference',          -- reference|working|archived
  created_at     INTEGER NOT NULL,
  CHECK (
    (task_id IS NOT NULL) OR
    (unit_id IS NOT NULL) OR
    (plan_id IS NOT NULL)
  )
);
CREATE INDEX IF NOT EXISTS idx_artifacts_task  ON artifacts(task_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_unit  ON artifacts(unit_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_plan  ON artifacts(plan_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_type  ON artifacts(type);
CREATE INDEX IF NOT EXISTS idx_artifacts_scope ON artifacts(scope);

CREATE TABLE IF NOT EXISTS artifact_versions (
  id             TEXT PRIMARY KEY,
  artifact_id    TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  version        INTEGER NOT NULL,
  content        TEXT,
  content_format TEXT,
  created_at     INTEGER,
  created_by     TEXT
);
CREATE INDEX IF NOT EXISTS idx_artifact_versions_artifact ON artifact_versions(artifact_id);

-- ============================================================
-- runs (execution log; one per sub-agent/session invocation)
-- ============================================================
CREATE TABLE IF NOT EXISTS runs (
  id         TEXT PRIMARY KEY,                               -- RUN-<ulid>
  task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  session_id TEXT,                                           -- Claude Code hook session_id
  agent      TEXT NOT NULL,                                  -- main|claude-sub:<type>|skill:<name>
  started_at INTEGER NOT NULL,
  ended_at   INTEGER,
  result     TEXT,                                           -- success|fail|aborted
  notes      TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_task    ON runs(task_id);
CREATE INDEX IF NOT EXISTS idx_runs_session ON runs(session_id);

-- ============================================================
-- questions (logged Q&A; prompts/web/hook origins)
-- ============================================================
CREATE TABLE IF NOT EXISTS questions (
  id          TEXT PRIMARY KEY,                              -- Q-<ulid>
  plan_id     TEXT REFERENCES plans(id) ON DELETE CASCADE,
  unit_id     TEXT REFERENCES units(id) ON DELETE CASCADE,
  task_id     TEXT REFERENCES tasks(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,                                 -- clarification|decision|blocker|review
  origin      TEXT NOT NULL,                                 -- prompt|web|hook
  body        TEXT NOT NULL,
  asked_by    TEXT,                                          -- main|human|skill:<name>
  created_at  INTEGER NOT NULL,
  answer      TEXT,
  answered_by TEXT,
  answered_at INTEGER,
  CHECK (
    (plan_id IS NOT NULL) OR
    (unit_id IS NOT NULL) OR
    (task_id IS NOT NULL)
  )
);
CREATE INDEX IF NOT EXISTS idx_questions_plan    ON questions(plan_id);
CREATE INDEX IF NOT EXISTS idx_questions_unit    ON questions(unit_id);
CREATE INDEX IF NOT EXISTS idx_questions_task    ON questions(task_id);
CREATE INDEX IF NOT EXISTS idx_questions_pending ON questions(answered_at) WHERE answered_at IS NULL;

-- ============================================================
-- activity_log (state-change audit)
-- ============================================================
CREATE TABLE IF NOT EXISTS activity_log (
  id          TEXT PRIMARY KEY,
  entity_type TEXT NOT NULL,                                 -- task|unit|cycle|plan|project
  entity_id   TEXT NOT NULL,
  action      TEXT NOT NULL,                                 -- status_change|created|deleted|updated
  field       TEXT,
  old_value   TEXT,
  new_value   TEXT,
  actor       TEXT,                                          -- agent name or 'human'
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_activity_log_entity ON activity_log(entity_type, entity_id);
CREATE INDEX IF NOT EXISTS idx_activity_log_time   ON activity_log(created_at DESC);

-- ============================================================
-- FTS5 virtual tables (deterministic search, no vectors)
-- Vector tables (vec_tasks, vec_artifacts) are provisioned at runtime
-- by the daemon only when sqlite-vec is available.
-- ============================================================
CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
  title, body,
  content='tasks',
  content_rowid='rowid',
  tokenize='unicode61'
);

CREATE VIRTUAL TABLE IF NOT EXISTS artifacts_fts USING fts5(
  title, content,
  content='artifacts',
  content_rowid='rowid',
  tokenize='unicode61'
);

-- FTS sync triggers
CREATE TRIGGER IF NOT EXISTS tasks_ai AFTER INSERT ON tasks BEGIN
  INSERT INTO tasks_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;
CREATE TRIGGER IF NOT EXISTS tasks_ad AFTER DELETE ON tasks BEGIN
  INSERT INTO tasks_fts(tasks_fts, rowid, title, body) VALUES('delete', old.rowid, old.title, old.body);
END;
CREATE TRIGGER IF NOT EXISTS tasks_au AFTER UPDATE ON tasks BEGIN
  INSERT INTO tasks_fts(tasks_fts, rowid, title, body) VALUES('delete', old.rowid, old.title, old.body);
  INSERT INTO tasks_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;

CREATE TRIGGER IF NOT EXISTS artifacts_ai AFTER INSERT ON artifacts BEGIN
  INSERT INTO artifacts_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;
CREATE TRIGGER IF NOT EXISTS artifacts_ad AFTER DELETE ON artifacts BEGIN
  INSERT INTO artifacts_fts(artifacts_fts, rowid, title, content) VALUES('delete', old.rowid, old.title, old.content);
END;
CREATE TRIGGER IF NOT EXISTS artifacts_au AFTER UPDATE ON artifacts BEGIN
  INSERT INTO artifacts_fts(artifacts_fts, rowid, title, content) VALUES('delete', old.rowid, old.title, old.content);
  INSERT INTO artifacts_fts(rowid, title, content) VALUES (new.rowid, new.title, new.content);
END;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (1, strftime('%s','now') * 1000);
