use crate::id::{new_id, now_ms};
use crate::models::ActivityLogEntry;
use anyhow::Result;
use rusqlite::{params, Connection};

pub struct RecordInput<'a> {
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub field: Option<&'a str>,
    pub old_value: Option<&'a str>,
    pub new_value: Option<&'a str>,
    pub actor: Option<&'a str>,
}

pub fn record(conn: &Connection, input: RecordInput<'_>) -> Result<ActivityLogEntry> {
    let id = new_id("LOG");
    let ts = now_ms();
    conn.execute(
        "INSERT INTO activity_log (id, entity_type, entity_id, action, field, old_value, new_value, actor, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, input.entity_type, input.entity_id, input.action, input.field, input.old_value, input.new_value, input.actor, ts],
    )?;
    Ok(ActivityLogEntry {
        id,
        entity_type: input.entity_type.to_string(),
        entity_id: input.entity_id.to_string(),
        action: input.action.to_string(),
        field: input.field.map(String::from),
        old_value: input.old_value.map(String::from),
        new_value: input.new_value.map(String::from),
        actor: input.actor.map(String::from),
        created_at: ts,
    })
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub entity_type: Option<&'a str>,
    pub entity_id: Option<&'a str>,
    pub limit: Option<i64>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<ActivityLogEntry>> {
    let mut sql = String::from(
        "SELECT id, entity_type, entity_id, action, field, old_value, new_value, actor, created_at FROM activity_log",
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
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    vals.push(filter.limit.unwrap_or(50).into());

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, |r| {
        Ok(ActivityLogEntry {
            id: r.get(0)?,
            entity_type: r.get(1)?,
            entity_id: r.get(2)?,
            action: r.get(3)?,
            field: r.get(4)?,
            old_value: r.get(5)?,
            new_value: r.get(6)?,
            actor: r.get(7)?,
            created_at: r.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
