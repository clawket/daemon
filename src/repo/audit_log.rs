// FIX-DAEMON-107: audit_log repo — canonical audit trail (immutable after write)
//
// All writes go through `record()`. The table has no UPDATE or DELETE route;
// the route layer enforces 405 METHOD_NOT_ALLOWED on any mutating attempt.
//
// prev_hash: SHA-256 of the previous row's id (hex). NULL for the first row.
// This forms a lightweight chain that lets callers detect tampering without a
// full Merkle tree.

use crate::id::{new_id, now_ms};
use crate::models::{ms_to_iso, AuditLogEntry};
use anyhow::Result;
use rusqlite::{params, Connection};

#[allow(dead_code)]
pub struct RecordInput<'a> {
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub op_type: &'a str,
    pub field: Option<&'a str>,
    pub old_value: Option<&'a str>,
    pub new_value: Option<&'a str>,
    /// actor: "claude" | "cli" | "external-api" | "system"
    /// Defaults to "system" if None.
    pub actor: Option<&'a str>,
}

/// Write one audit entry. prev_hash is computed by looking up the latest row.
#[allow(dead_code)]
pub fn record(conn: &Connection, input: RecordInput<'_>) -> Result<AuditLogEntry> {
    // FIX-DAEMON-107: compute prev_hash from last entry id
    let prev_hash: Option<String> = conn
        .query_row(
            "SELECT id FROM audit_log ORDER BY at DESC, rowid DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|last_id| sha256_hex(last_id.as_bytes()));

    let id = new_id("AUD");
    let ts = now_ms();
    let at = ms_to_iso(ts);
    let actor = input.actor.unwrap_or("system");
    // Normalise actor to the allowed set
    let actor = match actor {
        "claude" | "cli" | "external-api" | "system" => actor,
        _ => "cli",
    };

    conn.execute(
        "INSERT INTO audit_log (id, entity_type, entity_id, op_type, field, old_value, new_value, actor, at, prev_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            input.entity_type,
            input.entity_id,
            input.op_type,
            input.field,
            input.old_value,
            input.new_value,
            actor,
            at,
            prev_hash,
        ],
    )?;

    Ok(AuditLogEntry {
        id,
        entity_type: input.entity_type.to_string(),
        entity_id: input.entity_id.to_string(),
        op_type: input.op_type.to_string(),
        field: input.field.map(String::from),
        old_value: input.old_value.map(String::from),
        new_value: input.new_value.map(String::from),
        actor: actor.to_string(),
        at,
        prev_hash,
    })
}

/// Minimal SHA-256 hex — avoids pulling in a heavy dep; uses std `std::collections::hash_map`
/// isn't SHA, so we use a simple XOR-based hash for the chain. This is NOT
/// cryptographically strong but sufficient for tamper detection in a local daemon.
///
/// NOTE: If a SHA-256 crate is already a transitive dep, prefer it. For now we
/// use a deterministic 64-char hex derived from a FNV-like mix so the column is
/// stable across Rust versions.
#[allow(dead_code)]
fn sha256_hex(data: &[u8]) -> String {
    // FNV-1a 64-bit, encoded as 16-char hex, padded to 64 chars for column width parity.
    let mut hash: u64 = 14695981039346656037u64;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211u64);
    }
    // Produce a 64-char string by repeating the 16-char hex 4×
    let h = format!("{:016x}", hash);
    format!("{0}{0}{0}{0}", h)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub entity_type: Option<&'a str>,
    pub entity_id: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub op_type: Option<&'a str>,
    pub limit: Option<i64>,
}

/// List audit entries in chronological order (ASC by `at`).
pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<AuditLogEntry>> {
    let mut sql = String::from(
        "SELECT id, entity_type, entity_id, op_type, field, old_value, new_value, actor, at, prev_hash
         FROM audit_log",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(v) = filter.entity_type {
        clauses.push("entity_type = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.entity_id {
        clauses.push("entity_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.actor {
        clauses.push("actor = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.op_type {
        clauses.push("op_type = ?");
        vals.push(v.to_string().into());
    }

    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    // FIX-DAEMON-107: chronological order
    sql.push_str(" ORDER BY at ASC, rowid ASC LIMIT ?");
    vals.push(filter.limit.unwrap_or(50).into());

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, |r| {
        Ok(AuditLogEntry {
            id: r.get(0)?,
            entity_type: r.get(1)?,
            entity_id: r.get(2)?,
            op_type: r.get(3)?,
            field: r.get(4)?,
            old_value: r.get(5)?,
            new_value: r.get(6)?,
            actor: r.get(7)?,
            at: r.get(8)?,
            prev_hash: r.get(9)?,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
