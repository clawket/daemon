use crate::id::{new_id, now_ms};
use crate::models::TaskComment;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub fn create(
    conn: &Connection,
    task_id: &str,
    author: &str,
    body: &str,
) -> Result<Option<TaskComment>> {
    let id = new_id("CMT");
    let ts = now_ms();
    conn.execute(
        "INSERT INTO task_comments (id, task_id, author, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, task_id, author, body, ts],
    )?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<TaskComment>> {
    let c = conn
        .query_row(
            "SELECT id, task_id, author, body, created_at FROM task_comments WHERE id = ?1",
            params![id],
            |r| {
                Ok(TaskComment {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    author: r.get(2)?,
                    body: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(c)
}

pub fn list(conn: &Connection, task_id: &str) -> Result<Vec<TaskComment>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, author, body, created_at
         FROM task_comments WHERE task_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![task_id], |r| {
        Ok(TaskComment {
            id: r.get(0)?,
            task_id: r.get(1)?,
            author: r.get(2)?,
            body: r.get(3)?,
            created_at: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM task_comments WHERE id = ?1", params![id])?;
    Ok(())
}
