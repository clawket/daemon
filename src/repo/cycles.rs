use crate::id::{new_id, now_ms};
use crate::models::Cycle;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub project_id: &'a str,
    pub title: &'a str,
    pub goal: Option<&'a str>,
    pub idx: Option<i64>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Cycle>> {
    let id = new_id("CYC");
    let ts = now_ms();
    let idx = match input.idx {
        Some(i) => i,
        None => conn.query_row(
            "SELECT COALESCE(MAX(idx), -1) + 1 FROM cycles WHERE project_id = ?1",
            params![input.project_id],
            |r| r.get::<_, i64>(0),
        )?,
    };
    conn.execute(
        "INSERT INTO cycles (id, project_id, title, goal, idx, created_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planning')",
        params![id, input.project_id, input.title, input.goal, idx, ts],
    )
    .context("insert cycle")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Cycle>> {
    let c = conn
        .query_row(
            "SELECT id, project_id, idx, title, goal, created_at, started_at, ended_at, status
             FROM cycles WHERE id = ?1",
            params![id],
            map_cycle,
        )
        .optional()?;
    Ok(c)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub project_id: Option<&'a str>,
    pub status: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Cycle>> {
    let mut sql = String::from(
        "SELECT id, project_id, idx, title, goal, created_at, started_at, ended_at, status FROM cycles",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(pid) = filter.project_id {
        clauses.push("project_id = ?");
        vals.push(pid.to_string().into());
    }
    if let Some(status) = filter.status {
        clauses.push("status = ?");
        vals.push(status.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY idx");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_cycle)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[derive(Default)]
pub struct UpdateFields {
    pub title: Option<String>,
    pub goal: Option<Option<String>>,
    pub status: Option<String>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Cycle>> {
    if let Some(status) = &f.status {
        if !matches!(status.as_str(), "planning" | "active" | "completed") {
            bail!(
                "Invalid cycle status: \"{}\". Valid: planning, active, completed",
                status
            );
        }
        let current = get(conn, id)?;
        if let Some(c) = current {
            if c.status == "completed" && status != "completed" {
                bail!(
                    "Cycle \"{}\" is completed and cannot be restarted. Create a new cycle instead.",
                    c.title
                );
            }
        }
    }

    let mut sets: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(title) = f.title {
        sets.push("title = ?");
        vals.push(title.into());
    }
    if let Some(goal) = f.goal {
        sets.push("goal = ?");
        vals.push(match goal {
            Some(s) => s.into(),
            None => rusqlite::types::Value::Null,
        });
    }
    if let Some(status) = &f.status {
        sets.push("status = ?");
        vals.push(status.clone().into());
        if status == "active" {
            sets.push("started_at = COALESCE(started_at, ?)");
            vals.push(now_ms().into());
        } else if status == "completed" {
            sets.push("ended_at = ?");
            vals.push(now_ms().into());
        }
    }

    if sets.is_empty() {
        return get(conn, id);
    }

    vals.push(id.to_string().into());
    let sql = format!("UPDATE cycles SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn activate(conn: &Connection, id: &str) -> Result<Option<Cycle>> {
    update(
        conn,
        id,
        UpdateFields {
            status: Some("active".into()),
            ..Default::default()
        },
    )
}

pub fn complete(conn: &Connection, id: &str) -> Result<Option<Cycle>> {
    update(
        conn,
        id,
        UpdateFields {
            status: Some("completed".into()),
            ..Default::default()
        },
    )
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET cycle_id = NULL WHERE cycle_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM cycles WHERE id = ?1", params![id])?;
    Ok(())
}

fn map_cycle(r: &rusqlite::Row<'_>) -> rusqlite::Result<Cycle> {
    Ok(Cycle {
        id: r.get(0)?,
        project_id: r.get(1)?,
        idx: r.get(2)?,
        title: r.get(3)?,
        goal: r.get(4)?,
        created_at: r.get(5)?,
        started_at: r.get(6)?,
        ended_at: r.get(7)?,
        status: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::projects;

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.sqlite")).unwrap();
        let project = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: None,
                key: None,
            },
        )
        .unwrap()
        .unwrap();
        (dir, db, project.id)
    }

    #[test]
    fn lifecycle() {
        let (_d, db, pid) = setup();

        let c = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                title: "Cycle 1",
                goal: Some("first sprint"),
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(c.idx, 0);
        assert_eq!(c.status, "planning");
        assert!(c.started_at.is_none());

        let active = activate(&db.conn, &c.id).unwrap().unwrap();
        assert_eq!(active.status, "active");
        assert!(active.started_at.is_some());

        let done = complete(&db.conn, &c.id).unwrap().unwrap();
        assert_eq!(done.status, "completed");
        assert!(done.ended_at.is_some());

        let err = activate(&db.conn, &c.id).unwrap_err();
        assert!(err.to_string().contains("cannot be restarted"));

        let err2 = update(
            &db.conn,
            &c.id,
            UpdateFields {
                status: Some("bogus".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err2.to_string().contains("Invalid cycle status"));

        let all = list(
            &db.conn,
            ListFilter {
                project_id: Some(&pid),
                status: None,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 1);

        delete(&db.conn, &c.id).unwrap();
        assert!(get(&db.conn, &c.id).unwrap().is_none());
    }
}
