use crate::id::{new_id, now_ms};
use crate::models::TaskRelation;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub fn create(
    conn: &Connection,
    source_task_id: &str,
    target_task_id: &str,
    relation_type: &str,
) -> Result<TaskRelation> {
    let id = new_id("REL");
    let ts = now_ms();
    conn.execute(
        "INSERT INTO task_relations (id, source_task_id, target_task_id, relation_type, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, source_task_id, target_task_id, relation_type, ts],
    )?;
    Ok(TaskRelation {
        id,
        source_task_id: source_task_id.to_string(),
        target_task_id: target_task_id.to_string(),
        relation_type: relation_type.to_string(),
        created_at: ts,
    })
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<TaskRelation>> {
    let r = conn
        .query_row(
            "SELECT id, source_task_id, target_task_id, relation_type, created_at
             FROM task_relations WHERE id = ?1",
            params![id],
            map_rel,
        )
        .optional()?;
    Ok(r)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub task_id: Option<&'a str>,
    pub relation_type: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<TaskRelation>> {
    let mut sql = String::from(
        "SELECT id, source_task_id, target_task_id, relation_type, created_at FROM task_relations",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(t) = filter.task_id {
        clauses.push("(source_task_id = ? OR target_task_id = ?)");
        vals.push(t.to_string().into());
        vals.push(t.to_string().into());
    }
    if let Some(rt) = filter.relation_type {
        clauses.push("relation_type = ?");
        vals.push(rt.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_rel)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM task_relations WHERE id = ?1", params![id])?;
    Ok(())
}

fn map_rel(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRelation> {
    Ok(TaskRelation {
        id: r.get(0)?,
        source_task_id: r.get(1)?,
        target_task_id: r.get(2)?,
        relation_type: r.get(3)?,
        created_at: r.get(4)?,
    })
}
