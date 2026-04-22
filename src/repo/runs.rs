use crate::id::{new_id, now_ms};
use crate::models::Run;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub fn create(
    conn: &Connection,
    task_id: &str,
    session_id: Option<&str>,
    agent: &str,
) -> Result<Option<Run>> {
    let id = new_id("RUN");
    conn.execute(
        "INSERT INTO runs (id, task_id, session_id, agent, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, task_id, session_id, agent, now_ms()],
    )?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Run>> {
    let r = conn
        .query_row(
            "SELECT id, task_id, session_id, agent, started_at, ended_at, result, notes
             FROM runs WHERE id = ?1",
            params![id],
            map_run,
        )
        .optional()?;
    Ok(r)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub task_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Run>> {
    if let Some(pid) = filter.project_id {
        let mut stmt = conn.prepare(
            "SELECT r.id, r.task_id, r.session_id, r.agent, r.started_at, r.ended_at, r.result, r.notes
             FROM runs r
             JOIN tasks s ON s.id = r.task_id
             JOIN units u ON u.id = s.unit_id
             JOIN plans pl ON pl.id = u.plan_id
             WHERE pl.project_id = ?1
             ORDER BY r.started_at DESC",
        )?;
        let rows = stmt.query_map(params![pid], map_run)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        return Ok(out);
    }

    let mut sql = String::from(
        "SELECT id, task_id, session_id, agent, started_at, ended_at, result, notes FROM runs",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = filter.task_id {
        clauses.push("task_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.session_id {
        clauses.push("session_id = ?");
        vals.push(v.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY started_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_run)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn finish(
    conn: &Connection,
    id: &str,
    result: &str,
    notes: Option<&str>,
) -> Result<Option<Run>> {
    conn.execute(
        "UPDATE runs SET ended_at = ?1, result = ?2, notes = ?3 WHERE id = ?4",
        params![now_ms(), result, notes, id],
    )?;
    get(conn, id)
}

fn map_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        task_id: r.get(1)?,
        session_id: r.get(2)?,
        agent: r.get(3)?,
        started_at: r.get(4)?,
        ended_at: r.get(5)?,
        result: r.get(6)?,
        notes: r.get(7)?,
    })
}
