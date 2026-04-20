use crate::id::{new_id, now_ms};
use crate::models::Question;
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub plan_id: Option<&'a str>,
    pub unit_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub kind: &'a str,
    pub origin: &'a str,
    pub body: &'a str,
    pub asked_by: Option<&'a str>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Question>> {
    if input.plan_id.is_none() && input.unit_id.is_none() && input.task_id.is_none() {
        bail!("question requires plan_id, unit_id, or task_id");
    }
    let id = new_id("Q");
    let ts = now_ms();
    conn.execute(
        "INSERT INTO questions (id, plan_id, unit_id, task_id, kind, origin, body, asked_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, input.plan_id, input.unit_id, input.task_id, input.kind, input.origin, input.body, input.asked_by, ts],
    )?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Question>> {
    let q = conn
        .query_row(
            "SELECT id, plan_id, unit_id, task_id, kind, origin, body, asked_by, created_at,
                    answer, answered_by, answered_at
             FROM questions WHERE id = ?1",
            params![id],
            map_q,
        )
        .optional()?;
    Ok(q)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub plan_id: Option<&'a str>,
    pub unit_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub pending: Option<bool>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Question>> {
    let mut sql = String::from(
        "SELECT id, plan_id, unit_id, task_id, kind, origin, body, asked_by, created_at,
                answer, answered_by, answered_at FROM questions",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = filter.plan_id {
        clauses.push("plan_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.unit_id {
        clauses.push("unit_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.task_id {
        clauses.push("task_id = ?");
        vals.push(v.to_string().into());
    }
    match filter.pending {
        Some(true) => clauses.push("answered_at IS NULL"),
        Some(false) => clauses.push("answered_at IS NOT NULL"),
        None => {}
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_q)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn answer(
    conn: &Connection,
    id: &str,
    answer: &str,
    answered_by: Option<&str>,
) -> Result<Option<Question>> {
    let by = answered_by.unwrap_or("human");
    conn.execute(
        "UPDATE questions SET answer = ?1, answered_by = ?2, answered_at = ?3 WHERE id = ?4",
        params![answer, by, now_ms(), id],
    )?;
    get(conn, id)
}

fn map_q(r: &rusqlite::Row<'_>) -> rusqlite::Result<Question> {
    Ok(Question {
        id: r.get(0)?,
        plan_id: r.get(1)?,
        unit_id: r.get(2)?,
        task_id: r.get(3)?,
        kind: r.get(4)?,
        origin: r.get(5)?,
        body: r.get(6)?,
        asked_by: r.get(7)?,
        created_at: r.get(8)?,
        answer: r.get(9)?,
        answered_by: r.get(10)?,
        answered_at: r.get(11)?,
    })
}
