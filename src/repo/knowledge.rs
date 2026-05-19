use crate::id::{new_id, now_ms};
use crate::models::{Knowledge, KnowledgeVersion};
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
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Knowledge>> {
    // Every knowledge row must be attached to at least one of task/unit/plan.
    if input.task_id.is_none() && input.unit_id.is_none() && input.plan_id.is_none() {
        bail!("KNOWLEDGE_NEEDS_ATTACHMENT: knowledge requires at least one of: task_id, unit_id, plan_id");
    }
    let id = new_id("ART");
    let ts = now_ms();
    let content = input.content.unwrap_or("");
    let fmt = input.content_format.unwrap_or("md");

    // Wiki tree constraints: no self-parent, max depth 8.
    if let Some(parent_id) = input.parent_id {
        if parent_id == id.as_str() {
            bail!("SELF_PARENT: knowledge cannot be its own parent");
        }
        let depth: i64 = conn
            .query_row(
                "WITH RECURSIVE depth_walk(pid, d) AS (
               SELECT ?1, 1
               UNION ALL
               SELECT k.parent_id, d + 1
               FROM knowledge k
               JOIN depth_walk dw ON k.id = dw.pid
               WHERE k.parent_id IS NOT NULL AND d < 10
             )
             SELECT COALESCE(MAX(d), 0) FROM depth_walk",
                rusqlite::params![parent_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if depth >= 8 {
            bail!("WIKI_DEPTH_EXCEEDED: max wiki tree depth is 8");
        }
    }

    conn.execute(
        "INSERT INTO knowledge (id, task_id, unit_id, plan_id, type, title, content, content_format, created_at, parent_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![id, input.task_id, input.unit_id, input.plan_id, input.type_, input.title, content, fmt, ts, input.parent_id],
    )
    .context("insert knowledge")?;
    get(conn, &id)
}

/// Delete a knowledge row. Direct children are promoted to root (parent_id =
/// NULL) so the rest of the wiki tree remains reachable; grandchildren keep
/// their existing parents. The public name is preserved for callsite
/// stability, and the API still reports `{"soft": true}` to clients.
pub fn soft_delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE knowledge SET parent_id = NULL WHERE parent_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM knowledge WHERE id = ?1", params![id])?;
    Ok(())
}

/// Get wiki tree (children of a parent, recursive) up to depth 8.
pub fn wiki_tree(
    conn: &Connection,
    root_id: Option<&str>,
    plan_id: Option<&str>,
) -> Result<Vec<Knowledge>> {
    let sql = if let Some(rid) = root_id {
        format!(
            "WITH RECURSIVE tree(id, depth) AS (
               SELECT id, 0 FROM knowledge WHERE id = '{rid}'
               UNION ALL
               SELECT k.id, t.depth + 1
               FROM knowledge k
               JOIN tree t ON k.parent_id = t.id
               WHERE t.depth < 8
             )
             SELECT k.id, k.task_id, k.unit_id, k.plan_id, k.type, k.title, k.content,
                    k.content_format, k.parent_id, k.created_at,
                    k.wiki_idx, k.wiki_depth
             FROM knowledge k JOIN tree t ON k.id = t.id
             ORDER BY k.parent_id NULLS FIRST, k.wiki_idx"
        )
    } else if let Some(pid) = plan_id {
        format!(
            "SELECT k.id, k.task_id, k.unit_id, k.plan_id, k.type, k.title, k.content,
                    k.content_format, k.parent_id, k.created_at,
                    k.wiki_idx, k.wiki_depth
             FROM knowledge k
             WHERE k.plan_id = '{pid}'
             ORDER BY k.parent_id NULLS FIRST, k.wiki_idx"
        )
    } else {
        return Ok(Vec::new());
    };

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map([], map_knowledge_with_wiki)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Knowledge>> {
    let a = conn
        .query_row(
            "SELECT id, task_id, unit_id, plan_id, type, title, content, content_format,
                    parent_id, created_at, wiki_idx, wiki_depth
             FROM knowledge WHERE id = ?1",
            params![id],
            map_knowledge_with_wiki,
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
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Knowledge>> {
    let mut sql = String::from(
        "SELECT id, task_id, unit_id, plan_id, type, title, content, content_format,
                parent_id, created_at, wiki_idx, wiki_depth FROM knowledge",
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
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_knowledge_with_wiki)?;
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
    pub created_by: Option<String>,
    /// Explicit parent reparenting: `Some(Some(id))` sets, `Some(None)` clears,
    /// `None` leaves unchanged. Cycles and self-parent are rejected.
    pub parent_id: Option<Option<String>>,
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Knowledge>> {
    let existing = match get(conn, id)? {
        Some(a) => a,
        None => return Ok(None),
    };

    if let Some(Some(ref new_parent)) = f.parent_id {
        if new_parent == id {
            bail!("SELF_PARENT: knowledge cannot be its own parent");
        }
        // Walk up the new parent's chain to ensure 'id' doesn't appear (cycle detection).
        let would_cycle: bool = conn
            .query_row(
                "WITH RECURSIVE chain(pid) AS (
               SELECT ?1
               UNION ALL
               SELECT k.parent_id FROM knowledge k JOIN chain c ON k.id = c.pid
               WHERE k.parent_id IS NOT NULL
             )
             SELECT COUNT(*) FROM chain WHERE pid = ?2",
                rusqlite::params![new_parent, id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if would_cycle {
            bail!(
                "WIKI_CYCLE: setting parent_id='{}' on knowledge '{}' would create a cycle",
                new_parent,
                id
            );
        }
    }

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
    if let Some(pv) = f.parent_id {
        sets.push("parent_id = ?");
        match pv {
            Some(s) => vals.push(s.into()),
            None => vals.push(rusqlite::types::Value::Null),
        }
    }
    if sets.is_empty() {
        return Ok(Some(existing));
    }

    vals.push(id.to_string().into());
    let sql = format!("UPDATE knowledge SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;
    get(conn, id)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM knowledge WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn keyword_search(conn: &Connection, query: &str, limit: i64) -> Result<Vec<Knowledge>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = build_knowledge_fts_query(trimmed);
    let sql = "SELECT k.id FROM knowledge k JOIN knowledge_fts f ON k.rowid = f.rowid
         WHERE knowledge_fts MATCH ?1 ORDER BY rank LIMIT ?2";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let ids: Vec<String> = stmt
        .query_map(params![fts_query, limit], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    drop(stmt);
    let mut out = Vec::new();
    for id in ids {
        if let Some(a) = get(conn, &id)? {
            out.push(a);
        }
    }
    Ok(out)
}

fn build_knowledge_fts_query(trimmed: &str) -> String {
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

pub fn store_embedding(conn: &Connection, knowledge_id: &str, embedding: &[f32]) -> Result<()> {
    let bytes: &[u8] = bytemuck_f32_slice(embedding);
    conn.execute(
        "DELETE FROM vec_knowledge WHERE knowledge_id = ?1",
        params![knowledge_id],
    )
    .ok();
    conn.execute(
        "INSERT INTO vec_knowledge (knowledge_id, embedding) VALUES (?1, ?2)",
        params![knowledge_id, bytes],
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
) -> Result<Vec<(Knowledge, f32)>> {
    let bytes = bytemuck_f32_slice(embedding);
    let mut stmt = match conn.prepare(
        "SELECT knowledge_id, distance FROM vec_knowledge
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
    knowledge_id: &str,
    created_by: Option<&str>,
) -> Result<Option<KnowledgeVersion>> {
    let existing = match get(conn, knowledge_id)? {
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
    knowledge_id: &str,
    content: Option<&str>,
    content_format: Option<&str>,
    created_by: Option<&str>,
) -> Result<KnowledgeVersion> {
    let id = new_id("ARTV");
    let ts = now_ms();
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM knowledge_versions WHERE knowledge_id = ?1",
        params![knowledge_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO knowledge_versions (id, knowledge_id, version, content, content_format, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, knowledge_id, version, content, content_format, ts, created_by],
    )?;
    Ok(KnowledgeVersion {
        id,
        knowledge_id: knowledge_id.to_string(),
        version,
        content: content.map(String::from),
        content_format: content_format.map(String::from),
        created_at: Some(ts),
        created_by: created_by.map(String::from),
    })
}

pub fn list_versions(conn: &Connection, knowledge_id: &str) -> Result<Vec<KnowledgeVersion>> {
    let mut stmt = conn.prepare(
        "SELECT id, knowledge_id, version, content, content_format, created_at, created_by
         FROM knowledge_versions WHERE knowledge_id = ?1 ORDER BY version ASC",
    )?;
    let rows = stmt.query_map(params![knowledge_id], |r| {
        Ok(KnowledgeVersion {
            id: r.get(0)?,
            knowledge_id: r.get(1)?,
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

fn map_knowledge_with_wiki(r: &rusqlite::Row<'_>) -> rusqlite::Result<Knowledge> {
    let type_: String = r.get(4)?;
    let content: String = r.get(6)?;
    // For decision knowledge entries, parse `## Decision` and `## Outcome` body sections.
    let (decision_text, outcome) = if type_ == "decision" {
        (
            parse_md_section(&content, "Decision"),
            parse_md_section(&content, "Outcome"),
        )
    } else {
        (None, None)
    };
    Ok(Knowledge {
        id: r.get(0)?,
        task_id: r.get(1)?,
        unit_id: r.get(2)?,
        plan_id: r.get(3)?,
        type_,
        title: r.get(5)?,
        content,
        content_format: r.get(7)?,
        parent_id: r.get(8)?,
        created_at: r.get(9)?,
        wiki_idx: r.get(10).ok(),
        wiki_depth: r.get(11).ok(),
        decision_text,
        outcome,
    })
}

/// Extract a markdown section body. Looks for `## <heading>` and returns
/// trimmed text up to the next `## ` or EOF. Returns None if section absent.
fn parse_md_section(body: &str, heading: &str) -> Option<String> {
    let needle = format!("## {}", heading);
    let start = body.find(&needle)?;
    let after = &body[start + needle.len()..];
    // Skip the rest of the heading line
    let rest = match after.find('\n') {
        Some(p) => &after[p + 1..],
        None => return None,
    };
    let end = rest.find("\n## ").unwrap_or(rest.len());
    let text = rest[..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
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
            },
        )
        .unwrap()
        .unwrap();
        assert!(a.id.starts_with("ART-"));

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
