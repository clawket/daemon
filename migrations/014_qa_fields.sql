-- FIX-DAEMON-006: QA workflow fields on tasks table
-- Scenarios: U10 PDD-080..087/100..104/110..113

ALTER TABLE tasks ADD COLUMN qa_status TEXT CHECK (qa_status IN ('pass', 'defect', 'scenario_error')) DEFAULT NULL;
ALTER TABLE tasks ADD COLUMN scenario_id TEXT DEFAULT NULL;
ALTER TABLE tasks ADD COLUMN defect_task TEXT REFERENCES tasks(id) DEFAULT NULL;
ALTER TABLE tasks ADD COLUMN scenario_amendment TEXT DEFAULT NULL;

CREATE INDEX IF NOT EXISTS idx_tasks_qa_status ON tasks(qa_status) WHERE qa_status IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tasks_scenario ON tasks(scenario_id) WHERE scenario_id IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (14, strftime('%s','now') * 1000);
