use crate::id::{new_id, now_ms};
use crate::models::Plan;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub project_id: &'a str,
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub source: Option<&'a str>,
    pub source_path: Option<&'a str>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Plan>> {
    let id = new_id("PLAN");
    let ts = now_ms();
    let source = input.source.unwrap_or("manual");
    conn.execute(
        "INSERT INTO plans (id, project_id, title, description, source, source_path, created_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'draft')",
        params![id, input.project_id, input.title, input.description, source, input.source_path, ts],
    )
    .context("insert plan")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Plan>> {
    let plan = conn
        .query_row(
            "SELECT id, project_id, title, description, source, source_path, created_at, approved_at, status
             FROM plans WHERE id = ?1",
            params![id],
            map_plan,
        )
        .optional()?;
    Ok(plan)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub project_id: Option<&'a str>,
    pub status: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Plan>> {
    let mut sql = String::from(
        "SELECT id, project_id, title, description, source, source_path, created_at, approved_at, status FROM plans",
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
    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_plan)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[derive(Default)]
pub struct UpdateFields {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub status: Option<String>,
    pub approved_at: Option<Option<i64>>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Plan>> {
    if let Some(status) = &f.status {
        if !matches!(status.as_str(), "draft" | "active" | "completed") {
            bail!(
                "Invalid plan status: \"{}\". Valid: draft, active, completed",
                status
            );
        }
    }

    let mut sets: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(title) = f.title {
        sets.push("title = ?");
        vals.push(title.into());
    }
    if let Some(desc) = f.description {
        sets.push("description = ?");
        vals.push(match desc {
            Some(s) => s.into(),
            None => rusqlite::types::Value::Null,
        });
    }
    let activating = f.status.as_deref() == Some("active");
    if let Some(status) = f.status {
        sets.push("status = ?");
        vals.push(status.into());
    }
    if activating {
        if let Some(approved) = f.approved_at {
            sets.push("approved_at = ?");
            vals.push(match approved {
                Some(t) => t.into(),
                None => rusqlite::types::Value::Null,
            });
        }
    }

    if sets.is_empty() {
        return get(conn, id);
    }

    vals.push(id.to_string().into());
    let sql = format!("UPDATE plans SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn approve(conn: &Connection, id: &str) -> Result<Option<Plan>> {
    update(
        conn,
        id,
        UpdateFields {
            status: Some("active".into()),
            approved_at: Some(Some(now_ms())),
            ..Default::default()
        },
    )
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM plans WHERE id = ?1", params![id])?;
    Ok(())
}

fn map_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Plan> {
    Ok(Plan {
        id: r.get(0)?,
        project_id: r.get(1)?,
        title: r.get(2)?,
        description: r.get(3)?,
        source: r.get(4)?,
        source_path: r.get(5)?,
        created_at: r.get(6)?,
        approved_at: r.get(7)?,
        status: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::projects;

    fn tmp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.sqlite")).unwrap();
        (dir, db)
    }

    fn make_project(db: &mut Db) -> String {
        projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "Demo",
                description: None,
                cwd: None,
                key: None,
            },
        )
        .unwrap()
        .unwrap()
        .id
    }

    #[test]
    fn create_list_update_approve_delete() {
        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);

        let p = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                title: "v1",
                description: Some("first"),
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        assert!(p.id.starts_with("PLAN-"));
        assert_eq!(p.status, "draft");
        assert_eq!(p.source, "manual");

        let got = get(&db.conn, &p.id).unwrap().unwrap();
        assert_eq!(got.title, "v1");

        let all = list(
            &db.conn,
            ListFilter {
                project_id: Some(&pid),
                status: None,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 1);

        update(
            &db.conn,
            &p.id,
            UpdateFields {
                title: Some("v1.1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get(&db.conn, &p.id).unwrap().unwrap().title, "v1.1");

        let err = update(
            &db.conn,
            &p.id,
            UpdateFields {
                status: Some("bogus".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid plan status"));

        let approved = approve(&db.conn, &p.id).unwrap().unwrap();
        assert_eq!(approved.status, "active");
        assert!(approved.approved_at.is_some());

        let drafts = list(
            &db.conn,
            ListFilter {
                project_id: Some(&pid),
                status: Some("draft"),
            },
        )
        .unwrap();
        assert_eq!(drafts.len(), 0);

        delete(&db.conn, &p.id).unwrap();
        assert!(get(&db.conn, &p.id).unwrap().is_none());
    }
}
