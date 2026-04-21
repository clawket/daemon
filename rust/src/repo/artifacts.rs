use crate::id::{new_id, now_ms};
use crate::models::{Artifact, ArtifactVersion};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub task_id: Option<&'a str>,
    pub unit_id: Option<&'a str>,
    pub plan_id: Option<&'a str>,
    pub type_: &'a str,
    pub title: &'a str,
    pub content: Option<&'a str>,
    pub content_format: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    pub scope: Option<&'a str>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Artifact>> {
    if input.task_id.is_none() && input.unit_id.is_none() && input.plan_id.is_none() {
        bail!("artifact requires task_id, unit_id, or plan_id");
    }
    let id = new_id("ART");
    let ts = now_ms();
    let content = input.content.unwrap_or("");
    let fmt = input.content_format.unwrap_or("md");
    let scope = input.scope.unwrap_or("reference");
    conn.execute(
        "INSERT INTO artifacts (id, task_id, unit_id, plan_id, type, title, content, content_format, created_at, parent_id, scope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![id, input.task_id, input.unit_id, input.plan_id, input.type_, input.title, content, fmt, ts, input.parent_id, scope],
    )
    .context("insert artifact")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Artifact>> {
    let a = conn
        .query_row(
            "SELECT id, task_id, unit_id, plan_id, type, title, content, content_format,
                    parent_id, scope, created_at
             FROM artifacts WHERE id = ?1",
            params![id],
            map_artifact,
        )
        .optional()?;
    Ok(a)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub task_id: Option<&'a str>,
    pub unit_id: Option<&'a str>,
    pub plan_id: Option<&'a str>,
    pub type_: Option<&'a str>,
    pub scope: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Artifact>> {
    let mut sql = String::from(
        "SELECT id, task_id, unit_id, plan_id, type, title, content, content_format,
                parent_id, scope, created_at FROM artifacts",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = filter.task_id {
        clauses.push("task_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.unit_id {
        clauses.push("unit_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.plan_id {
        clauses.push("plan_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.type_ {
        clauses.push("type = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.scope {
        clauses.push("scope = ?");
        vals.push(v.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_artifact)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[derive(Default)]
pub struct UpdateFields {
    pub title: Option<String>,
    pub content: Option<String>,
    pub content_format: Option<String>,
    pub scope: Option<String>,
    pub created_by: Option<String>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Artifact>> {
    let existing = match get(conn, id)? {
        Some(a) => a,
        None => return Ok(None),
    };

    if let Some(new_content) = &f.content {
        if *new_content != existing.content {
            create_version(
                conn,
                &existing.id,
                Some(&existing.content),
                Some(&existing.content_format),
                f.created_by.as_deref(),
            )?;
        }
    }

    let mut sets: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = f.title {
        sets.push("title = ?");
        vals.push(v.into());
    }
    if let Some(v) = f.content {
        sets.push("content = ?");
        vals.push(v.into());
    }
    if let Some(v) = f.content_format {
        sets.push("content_format = ?");
        vals.push(v.into());
    }
    if let Some(v) = f.scope {
        sets.push("scope = ?");
        vals.push(v.into());
    }
    if sets.is_empty() {
        return Ok(Some(existing));
    }

    vals.push(id.to_string().into());
    let sql = format!("UPDATE artifacts SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM artifacts WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn keyword_search(
    conn: &Connection,
    query: &str,
    limit: i64,
    scope: Option<&str>,
) -> Result<Vec<Artifact>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = build_artifact_fts_query(trimmed);
    let sql = if scope.is_some() {
        "SELECT a.id FROM artifacts a JOIN artifacts_fts f ON a.rowid = f.rowid
         WHERE artifacts_fts MATCH ?1 AND a.scope = ?2 ORDER BY rank LIMIT ?3"
    } else {
        "SELECT a.id FROM artifacts a JOIN artifacts_fts f ON a.rowid = f.rowid
         WHERE artifacts_fts MATCH ?1 ORDER BY rank LIMIT ?2"
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let ids: Vec<String> = if let Some(s) = scope {
        stmt.query_map(params![fts_query, s, limit], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    } else {
        stmt.query_map(params![fts_query, limit], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };
    drop(stmt);
    let mut out = Vec::new();
    for id in ids {
        if let Some(a) = get(conn, &id)? {
            out.push(a);
        }
    }
    Ok(out)
}

fn build_artifact_fts_query(trimmed: &str) -> String {
    if trimmed
        .chars()
        .any(|c| matches!(c, '*' | '"' | ':' | '(' | ')'))
    {
        return trimmed.to_string();
    }
    let terms: Vec<String> = trimmed
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| format!("{t}*"))
        .collect();
    terms.join(" ")
}

pub fn store_embedding(conn: &Connection, artifact_id: &str, embedding: &[f32]) -> Result<()> {
    let bytes: &[u8] = bytemuck_f32_slice(embedding);
    conn.execute(
        "DELETE FROM vec_artifacts WHERE artifact_id = ?1",
        params![artifact_id],
    )
    .ok();
    conn.execute(
        "INSERT INTO vec_artifacts (artifact_id, embedding) VALUES (?1, ?2)",
        params![artifact_id, bytes],
    )
    .ok();
    Ok(())
}

fn bytemuck_f32_slice(v: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub fn vector_search(
    conn: &Connection,
    embedding: &[f32],
    limit: i64,
    scope: Option<&str>,
) -> Result<Vec<(Artifact, f32)>> {
    let bytes = bytemuck_f32_slice(embedding);
    let mut stmt = match conn.prepare(
        "SELECT artifact_id, distance FROM vec_artifacts
         WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let over_fetch = (limit * 2).max(limit);
    let rows = stmt.query_map(params![bytes, over_fetch], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, f32>(1)?))
    });
    let rows = match rows {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for row in rows {
        let (id, distance) = match row {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(art) = get(conn, &id)? {
            if let Some(s) = scope {
                if art.scope != s {
                    continue;
                }
            }
            out.push((art, distance));
            if (out.len() as i64) >= limit {
                break;
            }
        }
    }
    Ok(out)
}

pub fn snapshot_current(
    conn: &Connection,
    artifact_id: &str,
    created_by: Option<&str>,
) -> Result<Option<ArtifactVersion>> {
    let existing = match get(conn, artifact_id)? {
        Some(a) => a,
        None => return Ok(None),
    };
    let v = create_version(
        conn,
        &existing.id,
        Some(&existing.content),
        Some(&existing.content_format),
        created_by,
    )?;
    Ok(Some(v))
}

fn create_version(
    conn: &Connection,
    artifact_id: &str,
    content: Option<&str>,
    content_format: Option<&str>,
    created_by: Option<&str>,
) -> Result<ArtifactVersion> {
    let id = new_id("ARTV");
    let ts = now_ms();
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM artifact_versions WHERE artifact_id = ?1",
        params![artifact_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO artifact_versions (id, artifact_id, version, content, content_format, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, artifact_id, version, content, content_format, ts, created_by],
    )?;
    Ok(ArtifactVersion {
        id,
        artifact_id: artifact_id.to_string(),
        version,
        content: content.map(String::from),
        content_format: content_format.map(String::from),
        created_at: Some(ts),
        created_by: created_by.map(String::from),
    })
}

pub fn list_versions(conn: &Connection, artifact_id: &str) -> Result<Vec<ArtifactVersion>> {
    let mut stmt = conn.prepare(
        "SELECT id, artifact_id, version, content, content_format, created_at, created_by
         FROM artifact_versions WHERE artifact_id = ?1 ORDER BY version ASC",
    )?;
    let rows = stmt.query_map(params![artifact_id], |r| {
        Ok(ArtifactVersion {
            id: r.get(0)?,
            artifact_id: r.get(1)?,
            version: r.get(2)?,
            content: r.get(3)?,
            content_format: r.get(4)?,
            created_at: r.get(5)?,
            created_by: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn map_artifact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        id: r.get(0)?,
        task_id: r.get(1)?,
        unit_id: r.get(2)?,
        plan_id: r.get(3)?,
        type_: r.get(4)?,
        title: r.get(5)?,
        content: r.get(6)?,
        content_format: r.get(7)?,
        parent_id: r.get(8)?,
        scope: r.get(9)?,
        created_at: r.get(10)?,
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
                title: "pl",
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
    fn create_requires_parent() {
        let (_d, db, _) = setup();
        let err = create(
            &db.conn,
            CreateInput {
                task_id: None,
                unit_id: None,
                plan_id: None,
                type_: "note",
                title: "orphan",
                content: None,
                content_format: None,
                parent_id: None,
                scope: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires"));
    }

    #[test]
    fn crud_and_versioning() {
        let (_d, db, plan_id) = setup();
        let a = create(
            &db.conn,
            CreateInput {
                task_id: None,
                unit_id: None,
                plan_id: Some(&plan_id),
                type_: "decision",
                title: "ADR",
                content: Some("v1"),
                content_format: Some("md"),
                parent_id: None,
                scope: Some("reference"),
            },
        )
        .unwrap()
        .unwrap();
        assert!(a.id.starts_with("ART-"));
        assert_eq!(a.scope, "reference");

        update(
            &db.conn,
            &a.id,
            UpdateFields {
                content: Some("v2".into()),
                created_by: Some("tester".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let updated = get(&db.conn, &a.id).unwrap().unwrap();
        assert_eq!(updated.content, "v2");

        let versions = list_versions(&db.conn, &a.id).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[0].content.as_deref(), Some("v1"));

        let listed = list(
            &db.conn,
            ListFilter {
                plan_id: Some(&plan_id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(listed.len(), 1);

        delete(&db.conn, &a.id).unwrap();
        assert!(get(&db.conn, &a.id).unwrap().is_none());
    }
}
