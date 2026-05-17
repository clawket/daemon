-- FIX-DAEMON-001: Add tasks.tier column (low|med|high)
-- Scenarios: U14 TIER-001/002/004-010/020/022-024/040-044/050-054/070-072

ALTER TABLE tasks ADD COLUMN tier TEXT CHECK (tier IN ('low', 'med', 'high')) DEFAULT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (11, strftime('%s','now') * 1000);
