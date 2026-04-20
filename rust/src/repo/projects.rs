use crate::id::{now_ms, slugify};
use crate::models::Project;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub fn generate_key_from_name(name: &str) -> String {
    let words: Vec<&str> = name
        .trim()
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|w| !w.is_empty())
        .collect();
    if words.len() == 1 {
        words[0].chars().take(3).collect::<String>().to_uppercase()
    } else {
        words
            .iter()
            .take(4)
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_uppercase()
    }
}

pub struct CreateInput<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub key: Option<&'a str>,
}

pub fn create(conn: &mut Connection, input: CreateInput<'_>) -> Result<Option<Project>> {
    let id = format!("PROJ-{}", slugify(input.name));
    let ts = now_ms();
    let final_key = match input.key {
        Some(k) => k.to_uppercase(),
        None => generate_key_from_name(input.name),
    };

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO projects (id, name, description, created_at, updated_at, key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, input.name, input.description, ts, ts, final_key],
    )
    .context("insert project")?;
    if let Some(cwd) = input.cwd {
        tx.execute(
            "INSERT INTO project_cwds (project_id, cwd) VALUES (?1, ?2)",
            params![id, cwd],
        )?;
    }
    tx.commit()?;

    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Project>> {
    let row = conn
        .query_row(
            "SELECT id, name, description, key, enabled, wiki_paths, created_at, updated_at
             FROM projects WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;

    let Some((id, name, description, key, enabled, wiki_paths_json, created_at, updated_at)) = row
    else {
        return Ok(None);
    };

    let wiki_paths: Vec<String> = serde_json::from_str(&wiki_paths_json)
        .unwrap_or_else(|_| vec!["docs".to_string()]);
    let cwds = list_cwds(conn, &id)?;

    Ok(Some(Project {
        id,
        name,
        description,
        key,
        enabled,
        wiki_paths,
        cwds,
        created_at,
        updated_at,
    }))
}

pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<Project>> {
    let id: Option<String> = conn
        .query_row("SELECT id FROM projects WHERE name = ?1", params![name], |r| r.get(0))
        .optional()?;
    match id {
        Some(i) => get(conn, &i),
        None => Ok(None),
    }
}

pub fn get_by_cwd(conn: &Connection, cwd: &str, enabled_only: bool) -> Result<Option<Project>> {
    let exact_sql = if enabled_only {
        "SELECT p.id FROM projects p JOIN project_cwds c ON c.project_id = p.id
         WHERE c.cwd = ?1 AND p.enabled = 1 LIMIT 1"
    } else {
        "SELECT p.id FROM projects p JOIN project_cwds c ON c.project_id = p.id
         WHERE c.cwd = ?1 LIMIT 1"
    };
    let exact: Option<String> = conn
        .query_row(exact_sql, params![cwd], |r| r.get(0))
        .optional()?;
    if let Some(id) = exact {
        return get(conn, &id);
    }

    let prefix_sql = if enabled_only {
        "SELECT p.id FROM projects p JOIN project_cwds c ON c.project_id = p.id
         WHERE ?1 LIKE c.cwd || '/%' AND p.enabled = 1
         ORDER BY LENGTH(c.cwd) DESC LIMIT 1"
    } else {
        "SELECT p.id FROM projects p JOIN project_cwds c ON c.project_id = p.id
         WHERE ?1 LIKE c.cwd || '/%'
         ORDER BY LENGTH(c.cwd) DESC LIMIT 1"
    };
    let prefix: Option<String> = conn
        .query_row(prefix_sql, params![cwd], |r| r.get(0))
        .optional()?;
    match prefix {
        Some(id) => get(conn, &id),
        None => Ok(None),
    }
}

pub fn list(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, key, enabled, wiki_paths, created_at, updated_at
         FROM projects ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;

    let mut projects = Vec::new();
    for row in rows {
        let (id, name, description, key, enabled, wiki_paths_json, created_at, updated_at) = row?;
        let wiki_paths: Vec<String> =
            serde_json::from_str(&wiki_paths_json).unwrap_or_else(|_| vec!["docs".to_string()]);
        let cwds = list_cwds(conn, &id)?;
        projects.push(Project {
            id,
            name,
            description,
            key,
            enabled,
            wiki_paths,
            cwds,
            created_at,
            updated_at,
        });
    }
    Ok(projects)
}

pub fn add_cwd(conn: &Connection, id: &str, cwd: &str) -> Result<Option<Project>> {
    conn.execute(
        "INSERT OR IGNORE INTO project_cwds (project_id, cwd) VALUES (?1, ?2)",
        params![id, cwd],
    )?;
    get(conn, id)
}

pub fn remove_cwd(conn: &Connection, id: &str, cwd: &str) -> Result<Option<Project>> {
    conn.execute(
        "DELETE FROM project_cwds WHERE project_id = ?1 AND cwd = ?2",
        params![id, cwd],
    )?;
    get(conn, id)
}

#[derive(Default)]
pub struct UpdateFields {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub key: Option<Option<String>>,
    pub enabled: Option<i64>,
    pub wiki_paths: Option<Vec<String>>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Project>> {
    let mut sets: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(name) = f.name {
        sets.push("name = ?");
        vals.push(name.into());
    }
    if let Some(desc) = f.description {
        sets.push("description = ?");
        vals.push(match desc {
            Some(s) => s.into(),
            None => rusqlite::types::Value::Null,
        });
    }
    if let Some(key) = f.key {
        sets.push("key = ?");
        vals.push(match key {
            Some(k) => k.to_uppercase().into(),
            None => rusqlite::types::Value::Null,
        });
    }
    if let Some(enabled) = f.enabled {
        sets.push("enabled = ?");
        vals.push(enabled.into());
    }
    if let Some(wp) = f.wiki_paths {
        sets.push("wiki_paths = ?");
        vals.push(serde_json::to_string(&wp)?.into());
    }

    if sets.is_empty() {
        return get(conn, id);
    }

    sets.push("updated_at = ?");
    vals.push(now_ms().into());
    vals.push(id.to_string().into());

    let sql = format!("UPDATE projects SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    Ok(())
}

fn list_cwds(conn: &Connection, project_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT cwd FROM project_cwds WHERE project_id = ?1")?;
    let rows = stmt.query_map(params![project_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn tmp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.sqlite")).unwrap();
        (dir, db)
    }

    #[test]
    fn create_get_list_cwd_update_delete() {
        let (_d, mut db) = tmp_db();

        let p = create(
            &mut db.conn,
            CreateInput {
                name: "My App",
                description: Some("demo"),
                cwd: Some("/tmp/myapp"),
                key: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(p.id, "PROJ-my-app");
        assert_eq!(p.key.as_deref(), Some("MA"));
        assert_eq!(p.cwds, vec!["/tmp/myapp".to_string()]);

        let by_name = get_by_name(&db.conn, "My App").unwrap().unwrap();
        assert_eq!(by_name.id, p.id);

        let by_cwd = get_by_cwd(&db.conn, "/tmp/myapp", false).unwrap().unwrap();
        assert_eq!(by_cwd.id, p.id);

        let by_subdir = get_by_cwd(&db.conn, "/tmp/myapp/sub", false).unwrap().unwrap();
        assert_eq!(by_subdir.id, p.id);

        add_cwd(&db.conn, &p.id, "/tmp/other").unwrap();
        let p2 = get(&db.conn, &p.id).unwrap().unwrap();
        assert_eq!(p2.cwds.len(), 2);

        remove_cwd(&db.conn, &p.id, "/tmp/other").unwrap();
        let p3 = get(&db.conn, &p.id).unwrap().unwrap();
        assert_eq!(p3.cwds.len(), 1);

        update(
            &db.conn,
            &p.id,
            UpdateFields {
                description: Some(Some("updated".into())),
                enabled: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let p4 = get(&db.conn, &p.id).unwrap().unwrap();
        assert_eq!(p4.description.as_deref(), Some("updated"));
        assert_eq!(p4.enabled, 0);

        assert_eq!(list(&db.conn).unwrap().len(), 1);

        delete(&db.conn, &p.id).unwrap();
        assert!(get(&db.conn, &p.id).unwrap().is_none());
    }

    #[test]
    fn generate_key() {
        assert_eq!(generate_key_from_name("tradingbot"), "TRA");
        assert_eq!(generate_key_from_name("my app"), "MA");
        assert_eq!(generate_key_from_name("one two three four five"), "OTTF");
        assert_eq!(generate_key_from_name("x_y-z"), "XYZ");
    }
}

