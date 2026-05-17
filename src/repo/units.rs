// FIX-DAEMON-004: Units are pure grouping entities — no status/approval lifecycle.
use crate::id::{new_id, now_ms};
use crate::models::Unit;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub plan_id: &'a str,
    pub title: &'a str,
    pub goal: Option<&'a str>,
    pub idx: Option<i64>,
    pub execution_mode: Option<&'a str>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Unit>> {
    let id = new_id("UNIT");
    let ts = now_ms();
    let idx = match input.idx {
        Some(i) => i,
        None => conn.query_row(
            "SELECT COALESCE(MAX(idx), -1) + 1 FROM units WHERE plan_id = ?1",
            params![input.plan_id],
            |r| r.get::<_, i64>(0),
        )?,
    };
    let mode = input.execution_mode.unwrap_or("sequential");
    conn.execute(
        "INSERT INTO units (id, plan_id, idx, title, goal, created_at, execution_mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, input.plan_id, idx, input.title, input.goal, ts, mode],
    )
    .context("insert unit")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Unit>> {
    let unit = conn
        .query_row(
            "SELECT id, plan_id, idx, title, goal, execution_mode, created_at
             FROM units WHERE id = ?1",
            params![id],
            map_unit,
        )
        .optional()?;
    Ok(unit)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub plan_id: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Unit>> {
    let mut sql = String::from(
        "SELECT id, plan_id, idx, title, goal, execution_mode, created_at
         FROM units",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(pid) = filter.plan_id {
        clauses.push("plan_id = ?");
        vals.push(pid.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY plan_id, idx");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_unit)?;
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
    pub execution_mode: Option<String>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Unit>> {
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
    if let Some(mode) = f.execution_mode {
        sets.push("execution_mode = ?");
        vals.push(mode.into());
    }

    if sets.is_empty() {
        return get(conn, id);
    }

    vals.push(id.to_string().into());
    let sql = format!("UPDATE units SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM units WHERE id = ?1", params![id])?;
    Ok(())
}

fn map_unit(r: &rusqlite::Row<'_>) -> rusqlite::Result<Unit> {
    Ok(Unit {
        id: r.get(0)?,
        plan_id: r.get(1)?,
        idx: r.get(2)?,
        title: r.get(3)?,
        goal: r.get(4)?,
        execution_mode: r.get(5)?,
        created_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects};

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
        let plan = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &project.id,
                title: "P1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        (dir, db, plan.id)
    }

    #[test]
    fn create_autoidx_list_update_delete() {
        let (_d, db, plan_id) = setup();

        let u1 = create(
            &db.conn,
            CreateInput {
                plan_id: &plan_id,
                title: "U1",
                goal: Some("first"),
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(u1.idx, 0);
        assert_eq!(u1.execution_mode, "sequential");

        let u2 = create(
            &db.conn,
            CreateInput {
                plan_id: &plan_id,
                title: "U2",
                goal: None,
                idx: None,
                execution_mode: Some("parallel"),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(u2.idx, 1);
        assert_eq!(u2.execution_mode, "parallel");

        let all = list(
            &db.conn,
            ListFilter {
                plan_id: Some(&plan_id),
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, u1.id);

        update(
            &db.conn,
            &u1.id,
            UpdateFields {
                title: Some("U1 renamed".into()),
                goal: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let u1b = get(&db.conn, &u1.id).unwrap().unwrap();
        assert_eq!(u1b.title, "U1 renamed");
        assert!(u1b.goal.is_none());

        delete(&db.conn, &u2.id).unwrap();
        assert!(get(&db.conn, &u2.id).unwrap().is_none());
    }
}
