use anyhow::Result;
use rusqlite::{Connection, ToSql};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Default)]
pub struct ListFilter<'a> {
    pub project_id: &'a str,
    pub limit: i64,
    pub offset: i64,
    pub types: Option<&'a str>,
}

#[derive(Serialize)]
pub struct TimelineEvent {
    pub id: String,
    pub event_type: String,
    pub entity_type: String,
    pub entity_id: Option<String>,
    pub entity_title: String,
    pub actor: Option<String>,
    pub created_at: Option<i64>,
    pub detail: Value,
}

pub fn list(conn: &Connection, f: ListFilter<'_>) -> Result<Vec<TimelineEvent>> {
    let type_set: Option<HashSet<String>> = f
        .types
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect());

    let wants = |t: &str| match &type_set {
        None => true,
        Some(set) => set.contains(t),
    };

    let mut parts: Vec<String> = Vec::new();
    let mut vals: Vec<Box<dyn ToSql>> = Vec::new();

    if wants("status_change") || wants("created") || wants("updated") || wants("assignment") {
        parts.push(
            r"SELECT al.id,
                CASE WHEN al.field = 'assignee' THEN 'assignment' ELSE al.action END AS event_type,
                al.entity_type, al.entity_id,
                COALESCE(s.title, '') AS entity_title,
                al.actor, al.created_at,
                al.field AS detail_field,
                al.old_value AS detail_old_value,
                al.new_value AS detail_new_value,
                NULL AS detail_body,
                NULL AS detail_artifact_type,
                NULL AS detail_agent,
                NULL AS detail_duration_ms,
                NULL AS detail_result
             FROM activity_log al
             LEFT JOIN tasks s ON al.entity_type = 'task' AND al.entity_id = s.id
             LEFT JOIN units ph ON s.unit_id = ph.id
             LEFT JOIN plans pl ON ph.plan_id = pl.id
             WHERE pl.project_id = ?"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
    }

    if wants("comment") {
        parts.push(
            r"SELECT cmt.id, 'comment' AS event_type, 'task' AS entity_type,
                cmt.task_id AS entity_id,
                COALESCE(s.title, '') AS entity_title,
                cmt.author AS actor, cmt.created_at,
                NULL AS detail_field, NULL AS detail_old_value, NULL AS detail_new_value,
                cmt.body AS detail_body,
                NULL AS detail_artifact_type, NULL AS detail_agent,
                NULL AS detail_duration_ms, NULL AS detail_result
             FROM task_comments cmt
             JOIN tasks s ON cmt.task_id = s.id
             JOIN units ph ON s.unit_id = ph.id
             JOIN plans pl ON ph.plan_id = pl.id
             WHERE pl.project_id = ?"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
    }

    if wants("artifact") {
        parts.push(
            r"SELECT art.id, 'artifact' AS event_type, 'task' AS entity_type,
                COALESCE(art.task_id, art.unit_id, art.plan_id) AS entity_id,
                COALESCE(s.title, ph2.title, pl2.title, '') AS entity_title,
                NULL AS actor, art.created_at,
                NULL AS detail_field, NULL AS detail_old_value, NULL AS detail_new_value,
                art.title AS detail_body,
                art.type AS detail_artifact_type,
                NULL AS detail_agent,
                NULL AS detail_duration_ms, NULL AS detail_result
             FROM artifacts art
             LEFT JOIN tasks s ON art.task_id = s.id
             LEFT JOIN units ph ON s.unit_id = ph.id
             LEFT JOIN plans pl ON ph.plan_id = pl.id
             LEFT JOIN units ph2 ON art.unit_id = ph2.id
             LEFT JOIN plans pl2 ON COALESCE(ph2.plan_id, art.plan_id) = pl2.id
             WHERE COALESCE(pl.project_id, pl2.project_id) = ?"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
    }

    if wants("run") {
        parts.push(
            r"SELECT r.id || ':start' AS id, 'run_start' AS event_type, 'task' AS entity_type,
                r.task_id AS entity_id,
                COALESCE(s.title, '') AS entity_title,
                r.agent AS actor, r.started_at AS created_at,
                NULL AS detail_field, NULL AS detail_old_value, NULL AS detail_new_value,
                NULL AS detail_body, NULL AS detail_artifact_type,
                r.agent AS detail_agent,
                NULL AS detail_duration_ms, NULL AS detail_result
             FROM runs r
             JOIN tasks s ON r.task_id = s.id
             JOIN units ph ON s.unit_id = ph.id
             JOIN plans pl ON ph.plan_id = pl.id
             WHERE pl.project_id = ?"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
        parts.push(
            r"SELECT r.id || ':end' AS id, 'run_end' AS event_type, 'task' AS entity_type,
                r.task_id AS entity_id,
                COALESCE(s.title, '') AS entity_title,
                r.agent AS actor, r.ended_at AS created_at,
                NULL AS detail_field, NULL AS detail_old_value, NULL AS detail_new_value,
                NULL AS detail_body, NULL AS detail_artifact_type,
                r.agent AS detail_agent,
                (r.ended_at - r.started_at) AS detail_duration_ms,
                r.result AS detail_result
             FROM runs r
             JOIN tasks s ON r.task_id = s.id
             JOIN units ph ON s.unit_id = ph.id
             JOIN plans pl ON ph.plan_id = pl.id
             WHERE pl.project_id = ? AND r.ended_at IS NOT NULL"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
    }

    if wants("question") {
        parts.push(
            r"SELECT q.id, 'question' AS event_type,
                CASE WHEN q.task_id IS NOT NULL THEN 'task'
                     WHEN q.unit_id IS NOT NULL THEN 'unit'
                     ELSE 'plan' END AS entity_type,
                COALESCE(q.task_id, q.unit_id, q.plan_id) AS entity_id,
                COALESCE(s.title, ph2.title, pl2.title, '') AS entity_title,
                q.asked_by AS actor, q.created_at,
                NULL AS detail_field, NULL AS detail_old_value, NULL AS detail_new_value,
                q.body AS detail_body,
                NULL AS detail_artifact_type, NULL AS detail_agent,
                NULL AS detail_duration_ms, NULL AS detail_result
             FROM questions q
             LEFT JOIN tasks s ON q.task_id = s.id
             LEFT JOIN units ph ON s.unit_id = ph.id
             LEFT JOIN plans pl ON ph.plan_id = pl.id
             LEFT JOIN units ph2 ON q.unit_id = ph2.id
             LEFT JOIN plans pl2 ON COALESCE(ph2.plan_id, q.plan_id) = pl2.id
             WHERE COALESCE(pl.project_id, pl2.project_id) = ?"
                .to_string(),
        );
        vals.push(Box::new(f.project_id.to_string()));
    }

    if parts.is_empty() {
        return Ok(Vec::new());
    }

    let sql = format!(
        "{}\nORDER BY created_at DESC\nLIMIT ? OFFSET ?",
        parts.join("\nUNION ALL\n")
    );
    vals.push(Box::new(f.limit));
    vals.push(Box::new(f.offset));

    let refs: Vec<&dyn ToSql> = vals.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let id: String = r.get(0)?;
        let event_type: String = r.get(1)?;
        let entity_type: String = r.get(2)?;
        let entity_id: Option<String> = r.get(3)?;
        let entity_title: String = r.get(4)?;
        let actor: Option<String> = r.get(5)?;
        let created_at: Option<i64> = r.get(6)?;
        let detail_field: Option<String> = r.get(7)?;
        let detail_old_value: Option<String> = r.get(8)?;
        let detail_new_value: Option<String> = r.get(9)?;
        let detail_body: Option<String> = r.get(10)?;
        let detail_artifact_type: Option<String> = r.get(11)?;
        let detail_agent: Option<String> = r.get(12)?;
        let detail_duration_ms: Option<i64> = r.get(13)?;
        let detail_result: Option<String> = r.get(14)?;

        let mut detail = Map::new();
        if let Some(v) = detail_field {
            detail.insert("field".into(), Value::String(v));
        }
        if let Some(v) = detail_old_value {
            detail.insert("old_value".into(), Value::String(v));
        }
        if let Some(v) = detail_new_value {
            detail.insert("new_value".into(), Value::String(v));
        }
        if let Some(v) = detail_body {
            detail.insert("body".into(), Value::String(v));
        }
        if let Some(v) = detail_artifact_type {
            detail.insert("artifact_type".into(), Value::String(v));
        }
        if let Some(v) = detail_agent {
            detail.insert("agent".into(), Value::String(v));
        }
        if let Some(v) = detail_duration_ms {
            detail.insert("duration_ms".into(), Value::Number(v.into()));
        }
        if let Some(v) = detail_result {
            detail.insert("result".into(), Value::String(v));
        }

        Ok(TimelineEvent {
            id,
            event_type,
            entity_type,
            entity_id,
            entity_title,
            actor,
            created_at,
            detail: Value::Object(detail),
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
