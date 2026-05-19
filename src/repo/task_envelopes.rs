// Routes consuming `list_for_task` / `set_active_on_task` land in LM-138 (envelope GET).
// `supersede` and version-bump live inside `create_for_task` once the route is wired.
#![allow(dead_code)]

use crate::id::{new_id, now_ms};
use crate::models::TaskEnvelope;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub task_id: &'a str,
    pub version: i64,
    pub json: &'a str,
    pub signed_by: &'a str,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<TaskEnvelope> {
    if input.version < 1 {
        bail!("Invalid envelope version: must be >= 1");
    }
    serde_json::from_str::<serde_json::Value>(input.json)
        .context("envelope json must be valid JSON")?;

    let id = new_id("ENV");
    let ts = now_ms();
    conn.execute(
        "INSERT INTO task_envelopes (id, task_id, version, json, signed_at, signed_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            input.task_id,
            input.version,
            input.json,
            ts,
            input.signed_by
        ],
    )
    .context("insert task_envelope")?;
    get(conn, &id)?.ok_or_else(|| anyhow::anyhow!("envelope vanished after insert"))
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<TaskEnvelope>> {
    let env = conn
        .query_row(
            "SELECT id, task_id, version, json, signed_at, signed_by, superseded_by
             FROM task_envelopes WHERE id = ?1",
            params![id],
            map_envelope,
        )
        .optional()?;
    Ok(env)
}

pub struct EnvelopeHistoryEntry {
    pub envelope: TaskEnvelope,
    /// `signed_at` of the envelope that superseded this one (None if still active).
    pub superseded_at: Option<i64>,
}

pub fn history_for_task(
    conn: &Connection,
    task_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<EnvelopeHistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.task_id, e.version, e.json, e.signed_at, e.signed_by, e.superseded_by,
                next.signed_at AS superseded_at
         FROM task_envelopes e
         LEFT JOIN task_envelopes next ON next.id = e.superseded_by
         WHERE e.task_id = ?1
         ORDER BY e.version DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![task_id, limit, offset], |r| {
        Ok(EnvelopeHistoryEntry {
            envelope: TaskEnvelope {
                id: r.get(0)?,
                task_id: r.get(1)?,
                version: r.get(2)?,
                json: r.get(3)?,
                signed_at: r.get(4)?,
                signed_by: r.get(5)?,
                superseded_by: r.get(6)?,
            },
            superseded_at: r.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn list_for_task(conn: &Connection, task_id: &str) -> Result<Vec<TaskEnvelope>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, version, json, signed_at, signed_by, superseded_by
         FROM task_envelopes WHERE task_id = ?1 ORDER BY version ASC, signed_at ASC",
    )?;
    let rows = stmt.query_map(params![task_id], map_envelope)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Return the envelope currently pointed to by `tasks.active_envelope_id`.
/// `tasks.active_envelope_id` is the single source of truth for "active" —
/// `clear_active_for_task` may set it to NULL while preserving envelope
/// rows for replay (per ADR-0001), so consulting `superseded_by IS NULL`
/// alone would falsely return a now-detached envelope.
pub fn active_for_task(conn: &Connection, task_id: &str) -> Result<Option<TaskEnvelope>> {
    let env = conn
        .query_row(
            "SELECT e.id, e.task_id, e.version, e.json, e.signed_at, e.signed_by, e.superseded_by
             FROM task_envelopes e
             JOIN tasks t ON t.active_envelope_id = e.id
             WHERE t.id = ?1",
            params![task_id],
            map_envelope,
        )
        .optional()?;
    Ok(env)
}

pub fn supersede(conn: &Connection, old_id: &str, new_id: &str) -> Result<()> {
    let updated = conn.execute(
        "UPDATE task_envelopes SET superseded_by = ?1 WHERE id = ?2 AND superseded_by IS NULL",
        params![new_id, old_id],
    )?;
    if updated == 0 {
        bail!("envelope not found or already superseded: {}", old_id);
    }
    Ok(())
}

pub fn set_active_on_task(conn: &Connection, task_id: &str, envelope_id: &str) -> Result<()> {
    let updated = conn.execute(
        "UPDATE tasks SET active_envelope_id = ?1 WHERE id = ?2",
        params![envelope_id, task_id],
    )?;
    if updated == 0 {
        // US-CLAWKET-I18N-040: route through the structured TASK_NOT_FOUND
        // code so error.rs From<anyhow::Error> maps to a localizable 404
        // (key: error.task.not_found).
        bail!("TASK_NOT_FOUND: task not found: {}", task_id);
    }
    Ok(())
}

/// Unlink the task's active envelope pointer without deleting any envelope rows.
/// Returns `true` if the pointer was non-null prior to the call (i.e. a clear
/// actually happened), `false` if the task already had no active envelope.
/// History rows in `task_envelopes` are preserved per ADR-0001 replayability.
pub fn clear_active_for_task(conn: &Connection, task_id: &str) -> Result<bool> {
    let prev: Option<Option<String>> = conn
        .query_row(
            "SELECT active_envelope_id FROM tasks WHERE id = ?1",
            params![task_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    // US-CLAWKET-I18N-040: localized via TASK_NOT_FOUND (error.task.not_found).
    let prev =
        prev.ok_or_else(|| anyhow::anyhow!("TASK_NOT_FOUND: task not found: {}", task_id))?;
    let was_active = prev.is_some();
    if was_active {
        conn.execute(
            "UPDATE tasks SET active_envelope_id = NULL WHERE id = ?1",
            params![task_id],
        )?;
    }
    Ok(was_active)
}

/// Sign a new envelope on a task: auto-increments version, supersedes the prior
/// active envelope (if any), and points `tasks.active_envelope_id` at the new
/// row. All four steps run in a transaction so a partial state can never land
/// on disk.
pub fn sign_for_task(
    conn: &mut Connection,
    task_id: &str,
    json: &str,
    signed_by: &str,
) -> Result<TaskEnvelope> {
    serde_json::from_str::<serde_json::Value>(json).context("envelope json must be valid JSON")?;

    let tx = conn.transaction()?;

    let prev_max: Option<i64> = tx
        .query_row(
            "SELECT MAX(version) FROM task_envelopes WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let next_version = prev_max.unwrap_or(0) + 1;

    let prev_active_id: Option<String> = tx
        .query_row(
            "SELECT id FROM task_envelopes
             WHERE task_id = ?1 AND superseded_by IS NULL
             ORDER BY signed_at DESC LIMIT 1",
            params![task_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;

    let id = new_id("ENV");
    let ts = now_ms();
    tx.execute(
        "INSERT INTO task_envelopes (id, task_id, version, json, signed_at, signed_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, task_id, next_version, json, ts, signed_by],
    )
    .context("insert task_envelope")?;

    if let Some(old_id) = prev_active_id {
        let updated = tx.execute(
            "UPDATE task_envelopes SET superseded_by = ?1 WHERE id = ?2 AND superseded_by IS NULL",
            params![id, old_id],
        )?;
        if updated == 0 {
            bail!("envelope superseded by another writer: {}", old_id);
        }
    }

    let task_updated = tx.execute(
        "UPDATE tasks SET active_envelope_id = ?1 WHERE id = ?2",
        params![id, task_id],
    )?;
    if task_updated == 0 {
        // US-CLAWKET-I18N-040: localized via TASK_NOT_FOUND (error.task.not_found).
        bail!("TASK_NOT_FOUND: task not found: {}", task_id);
    }

    tx.commit().context("commit envelope sign")?;

    get(conn, &id)?.ok_or_else(|| anyhow::anyhow!("envelope vanished after commit"))
}

fn map_envelope(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskEnvelope> {
    Ok(TaskEnvelope {
        id: r.get(0)?,
        task_id: r.get(1)?,
        version: r.get(2)?,
        json: r.get(3)?,
        signed_at: r.get(4)?,
        signed_by: r.get(5)?,
        superseded_by: r.get(6)?,
    })
}

#[derive(Debug, Clone)]
pub struct ChainEntry {
    pub task_id: String,
    pub depth: i64,
    pub json: Option<String>,
}

/// Resolve a task's envelope inheritance chain in a single recursive CTE
/// (RL-U3-11 / LM-64). Walks `parent_task_id` upward from the leaf and
/// joins each level's active envelope JSON via `tasks.active_envelope_id`.
///
/// Returned entries are ordered root → leaf (depth strictly decreasing,
/// leaf has depth 0). A `json: None` means that level has no active
/// envelope. Empty Vec means the leaf task itself does not exist.
///
/// `max_depth` is a paranoid safety bound — the cycle defense in
/// `repo::tasks` already rejects parent links that would form cycles, so
/// in practice the chain length matches the tree depth. 1024 is the
/// default used by the routes layer.
pub fn resolve_chain(conn: &Connection, task_id: &str, max_depth: i64) -> Result<Vec<ChainEntry>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(id, parent_task_id, active_envelope_id, depth) AS (
            SELECT id, parent_task_id, active_envelope_id, 0
            FROM tasks
            WHERE id = ?1
            UNION ALL
            SELECT t.id, t.parent_task_id, t.active_envelope_id, c.depth + 1
            FROM tasks t
            JOIN chain c ON t.id = c.parent_task_id
            WHERE c.depth < ?2
        )
        SELECT c.id, c.depth, e.json
        FROM chain c
        LEFT JOIN task_envelopes e ON e.id = c.active_envelope_id
        ORDER BY c.depth DESC",
    )?;
    let rows = stmt.query_map(params![task_id, max_depth], |r| {
        Ok(ChainEntry {
            task_id: r.get::<_, String>(0)?,
            depth: r.get::<_, i64>(1)?,
            json: r.get::<_, Option<String>>(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Deep-merge a chain of inherited envelope JSONs into a single Value
/// (root → leaf order, leaf wins). Skips levels with no active envelope.
/// Returns `Value::Null` if every level is None (no envelope anywhere on
/// the chain). Used by `/runs` to freeze a snapshot at execute time.
pub fn resolve_chain_active(chain: &[ChainEntry]) -> serde_json::Value {
    use serde_json::Value;
    let mut acc: Value = Value::Object(Default::default());
    let mut any = false;
    for entry in chain {
        if let Some(j) = &entry.json {
            if let Ok(v) = serde_json::from_str::<Value>(j) {
                deep_merge(&mut acc, &v);
                any = true;
            }
        }
    }
    if any {
        acc
    } else {
        Value::Null
    }
}

fn deep_merge(into: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value;
    match (into, patch) {
        (Value::Object(into_map), Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                deep_merge(into_map.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (slot, other) => *slot = other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, tasks, units};

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let mut db = Db::open(&path).unwrap();
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
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        plans::approve(&db.conn, &plan.id).unwrap();
        let unit = units::create(
            &db.conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: "U1",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        let task = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id: &unit.id,
                title: "T1",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
                atomic_size_hint: None,
                decomposition_policy: None,
                tier: None,
                qa_status: None,
                scenario_id: None,
                defect_task: None,
                scenario_amendment: None,
                evidence: None,
                batch_id: None,
            },
        )
        .unwrap()
        .unwrap();
        (dir, db, task.id)
    }

    #[test]
    fn create_and_get_envelope() {
        let (_d, db, task_id) = setup();
        let env = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{"version":1,"intent":"test"}"#,
                signed_by: "main",
            },
        )
        .unwrap();
        assert_eq!(env.task_id, task_id);
        assert_eq!(env.version, 1);
        assert_eq!(env.signed_by, "main");
        assert!(env.superseded_by.is_none());
        assert!(env.signed_at > 0);
        assert!(env.id.starts_with("ENV-"));

        let fetched = get(&db.conn, &env.id).unwrap().unwrap();
        assert_eq!(fetched.id, env.id);
    }

    #[test]
    fn rejects_invalid_json() {
        let (_d, db, task_id) = setup();
        let err = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: "not json",
                signed_by: "main",
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("valid JSON"));
    }

    #[test]
    fn rejects_zero_version() {
        let (_d, db, task_id) = setup();
        let err = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 0,
                json: r#"{}"#,
                signed_by: "main",
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Invalid envelope version"));
    }

    #[test]
    fn list_orders_by_version() {
        let (_d, db, task_id) = setup();
        let v1 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{"v":1}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        let v2 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 2,
                json: r#"{"v":2}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        let list = list_for_task(&db.conn, &task_id).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, v1.id);
        assert_eq!(list[1].id, v2.id);
    }

    #[test]
    fn active_returns_pointed_envelope() {
        let (_d, db, task_id) = setup();
        let v1 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{"v":1}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        set_active_on_task(&db.conn, &task_id, &v1.id).unwrap();
        let active1 = active_for_task(&db.conn, &task_id).unwrap().unwrap();
        assert_eq!(active1.id, v1.id);

        let v2 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 2,
                json: r#"{"v":2}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        supersede(&db.conn, &v1.id, &v2.id).unwrap();
        set_active_on_task(&db.conn, &task_id, &v2.id).unwrap();
        let active2 = active_for_task(&db.conn, &task_id).unwrap().unwrap();
        assert_eq!(active2.id, v2.id);
    }

    #[test]
    fn supersede_twice_fails() {
        let (_d, db, task_id) = setup();
        let v1 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        let v2 = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 2,
                json: r#"{}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        supersede(&db.conn, &v1.id, &v2.id).unwrap();
        let err = supersede(&db.conn, &v1.id, &v2.id).unwrap_err();
        assert!(err.to_string().contains("already superseded"));
    }

    #[test]
    fn set_active_on_task_updates_pointer() {
        let (_d, db, task_id) = setup();
        let env = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        set_active_on_task(&db.conn, &task_id, &env.id).unwrap();
        let stored: String = db
            .conn
            .query_row(
                "SELECT active_envelope_id FROM tasks WHERE id = ?1",
                params![&task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, env.id);
    }

    #[test]
    fn clear_active_unlinks_pointer_and_preserves_history() {
        let (_d, mut db, task_id) = setup();
        let env = sign_for_task(&mut db.conn, &task_id, r#"{"intent":"x"}"#, "a").unwrap();
        let before: Option<String> = db
            .conn
            .query_row(
                "SELECT active_envelope_id FROM tasks WHERE id = ?1",
                params![&task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, Some(env.id.clone()));

        let was = clear_active_for_task(&db.conn, &task_id).unwrap();
        assert!(was, "first clear should report was_active=true");

        let after: Option<String> = db
            .conn
            .query_row(
                "SELECT active_envelope_id FROM tasks WHERE id = ?1",
                params![&task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(after.is_none(), "active pointer should be NULL after clear");

        let history_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_envelopes WHERE task_id = ?1",
                params![&task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            history_count, 1,
            "envelope row must remain after clear (replayability)"
        );

        let was_again = clear_active_for_task(&db.conn, &task_id).unwrap();
        assert!(!was_again, "idempotent second clear reports false");
    }

    #[test]
    fn clear_active_unknown_task_errors() {
        let (_d, db, _task_id) = setup();
        let err = clear_active_for_task(&db.conn, "TASK-NOPE").unwrap_err();
        assert!(format!("{err:#}").contains("not found"));
    }

    #[test]
    fn unique_task_version_enforced() {
        let (_d, db, task_id) = setup();
        create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{}"#,
                signed_by: "a",
            },
        )
        .unwrap();
        let err = create(
            &db.conn,
            CreateInput {
                task_id: &task_id,
                version: 1,
                json: r#"{}"#,
                signed_by: "a",
            },
        )
        .unwrap_err();
        let chain = format!("{:#}", err).to_lowercase();
        assert!(
            chain.contains("unique") || chain.contains("constraint"),
            "expected uniqueness violation, got: {:#}",
            err
        );
    }

    fn make_unit(db: &mut Db) -> String {
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
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
        plans::approve(&db.conn, &plan.id).unwrap();
        let unit = units::create(
            &db.conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: "U",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        unit.id
    }

    fn make_chain(db: &mut Db, unit_id: &str, depth: usize) -> Vec<String> {
        let mut ids = Vec::with_capacity(depth);
        let mut parent: Option<String> = None;
        for i in 0..depth {
            let pid = parent.clone();
            let task = tasks::create(
                &mut db.conn,
                tasks::CreateInput {
                    unit_id,
                    title: &format!("T{}", i),
                    body: None,
                    assignee: None,
                    idx: None,
                    depends_on: vec![],
                    parent_task_id: pid.as_deref(),
                    priority: None,
                    complexity: None,
                    estimated_edits: None,
                    cycle_id: None,
                    reporter: None,
                    type_: None,
                    atomic_size_hint: None,
                    decomposition_policy: None,
                    tier: None,
                    qa_status: None,
                    scenario_id: None,
                    defect_task: None,
                    scenario_amendment: None,
                    evidence: None,
                    batch_id: None,
                },
            )
            .unwrap()
            .unwrap();
            sign_for_task(
                &mut db.conn,
                &task.id,
                &format!(r#"{{"version":1,"intent":"L{}","level_{}":true}}"#, i, i),
                "test",
            )
            .unwrap();
            parent = Some(task.id.clone());
            ids.push(task.id);
        }
        ids
    }

    #[test]
    fn resolve_chain_returns_root_to_leaf_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let unit_id = make_unit(&mut db);
        let ids = make_chain(&mut db, &unit_id, 3);
        let leaf = ids.last().unwrap();
        let chain = resolve_chain(&db.conn, leaf, 1024).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].task_id, ids[0]); // root first
        assert_eq!(chain[2].task_id, ids[2]); // leaf last
        assert_eq!(chain[2].depth, 0);
        assert_eq!(chain[0].depth, 2);
        for entry in &chain {
            assert!(entry.json.is_some());
        }
    }

    #[test]
    fn resolve_chain_caps_at_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let unit_id = make_unit(&mut db);
        let ids = make_chain(&mut db, &unit_id, 5);
        let leaf = ids.last().unwrap();
        let chain = resolve_chain(&db.conn, leaf, 2).unwrap();
        // depth 0 (leaf) + depth 1 + depth 2 = 3 rows when cap is `< 2`
        // produces leaf and one ancestor only. (`c.depth < ?2` means
        // depth 2 is the largest that may extend; depth+1 = 2 stops.)
        assert!(chain.len() <= 3, "chain too long: {}", chain.len());
    }

    #[test]
    fn resolve_chain_returns_empty_for_unknown_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).unwrap();
        let chain = resolve_chain(&db.conn, "TASK-NOPE", 1024).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn resolve_chain_includes_levels_with_no_envelope_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let unit_id = make_unit(&mut db);
        // Build 2 tasks parent → child where ONLY the parent has an envelope.
        let parent_task = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id: &unit_id,
                title: "parent",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
                atomic_size_hint: None,
                decomposition_policy: None,
                tier: None,
                qa_status: None,
                scenario_id: None,
                defect_task: None,
                scenario_amendment: None,
                evidence: None,
                batch_id: None,
            },
        )
        .unwrap()
        .unwrap();
        sign_for_task(&mut db.conn, &parent_task.id, r#"{"version":1}"#, "t").unwrap();
        let child = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id: &unit_id,
                title: "child",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: Some(&parent_task.id),
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: None,
                reporter: None,
                type_: None,
                atomic_size_hint: None,
                decomposition_policy: None,
                tier: None,
                qa_status: None,
                scenario_id: None,
                defect_task: None,
                scenario_amendment: None,
                evidence: None,
                batch_id: None,
            },
        )
        .unwrap()
        .unwrap();
        let chain = resolve_chain(&db.conn, &child.id, 1024).unwrap();
        assert_eq!(chain.len(), 2);
        assert!(chain[0].json.is_some(), "root has envelope");
        assert!(chain[1].json.is_none(), "leaf has no envelope");
    }

    #[test]
    fn resolve_chain_ten_deep_under_twenty_ms() {
        // RL-U3-11 success criterion: 10-depth resolve < 20ms. Run a
        // warm-up to stabilise the prepared-statement cache, then time
        // a single resolve. The CTE walks parent chain in one round
        // trip — well under budget on commodity hardware.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let unit_id = make_unit(&mut db);
        let ids = make_chain(&mut db, &unit_id, 10);
        let leaf = ids.last().unwrap();
        // warm-up
        let _ = resolve_chain(&db.conn, leaf, 1024).unwrap();
        let started = std::time::Instant::now();
        let chain = resolve_chain(&db.conn, leaf, 1024).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(chain.len(), 10);
        assert!(
            elapsed.as_millis() < 20,
            "10-depth resolve took {:?}, must be < 20ms",
            elapsed
        );
    }
}
