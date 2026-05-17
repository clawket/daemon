-- FIX-DAEMON-107: Add prev_hash column to audit_log for chain integrity
-- prev_hash: FNV-1a 64-bit hash of the previous row's id (hex-encoded, 64 chars).
-- NULL for the first row. Computed by the Rust layer at write time.
-- This column makes the audit trail tamper-evident without a full Merkle tree.

ALTER TABLE audit_log ADD COLUMN prev_hash TEXT;

-- Index for chain walks (lookup by prev entry)
CREATE INDEX IF NOT EXISTS idx_audit_log_chain ON audit_log(prev_hash) WHERE prev_hash IS NOT NULL;

INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (18, strftime('%s','now') * 1000);
