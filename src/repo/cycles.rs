// FIX-DAEMON-002: cycles.unit_id NOT NULL + FK to units(id)
// A4 axiom: 1 Cycle ⊂ 1 Unit. unit_id required on create; daemon enforces.
use crate::id::{new_id, now_ms};
use crate::models::Cycle;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct CreateInput<'a> {
    pub project_id: &'a str,
    pub unit_id: &'a str,
    pub title: &'a str,
    pub goal: Option<&'a str>,
    pub idx: Option<i64>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Cycle>> {
    if input.unit_id.is_empty() {
        bail!("unit_id is required for cycle creation (PDD A4: Cycle ⊂ Unit)");
    }
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
    // API-CYCLE-005: cycles in the same unit must be planned in order. Reject creating
    // a cycle with idx N when an existing planning cycle already has idx > N.
    let blocking_idx: Option<i64> = conn
        .query_row(
            "SELECT idx FROM cycles
             WHERE unit_id = ?1 AND status = 'planning' AND idx > ?2
             ORDER BY idx DESC LIMIT 1",
            params![input.unit_id, idx],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(other) = blocking_idx {
        bail!(
            "INVALID_REQUEST: previous cycles must be planned in order (unit has a planning cycle at idx={}, new idx={} is out of order)",
            other,
            idx
        );
    }
    conn.execute(
        "INSERT INTO cycles (id, project_id, unit_id, title, goal, idx, created_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planning')",
        params![
            id,
            input.project_id,
            input.unit_id,
            input.title,
            input.goal,
            idx,
            ts
        ],
    )
    .context("insert cycle")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Cycle>> {
    let c = conn
        .query_row(
            "SELECT id, project_id, unit_id, idx, title, goal, created_at, started_at, ended_at, status
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
    pub unit_id: Option<&'a str>,
    pub status: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Cycle>> {
    let mut sql = String::from(
        "SELECT id, project_id, unit_id, idx, title, goal, created_at, started_at, ended_at, status FROM cycles",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(pid) = filter.project_id {
        clauses.push("project_id = ?");
        vals.push(pid.to_string().into());
    }
    if let Some(uid) = filter.unit_id {
        clauses.push("unit_id = ?");
        vals.push(uid.to_string().into());
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
            if status == "completed" && c.status != "completed" {
                assert_no_todo_residue(conn, id)?;
            }
            // A4/A8: same-unit cycle serialization — reject if same unit already has active cycle
            if status == "active" {
                if let Some(uid) = &c.unit_id {
                    let active_count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM cycles WHERE unit_id = ?1 AND status = 'active' AND id != ?2",
                        params![uid, id],
                        |r| r.get(0),
                    )?;
                    if active_count > 0 {
                        bail!(
                            "UNIT_HAS_ACTIVE_CYCLE: unit {} already has an active cycle (PDD A8: same-unit cycles are serial). Complete the existing active cycle first.",
                            uid
                        );
                    }
                }
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
    // US-CLAWKET-API-CYCLE-005: activating cycle C in unit U auto-deactivates
    // any prior active cycle in the same unit by completing it. Same-unit
    // cycles are serial (PDD A8); the previous behavior of rejecting the
    // activation forced callers to script a two-step transition. We collapse
    // it into a single atomic operation here.
    if let Some(current) = get(conn, id)? {
        if let Some(uid) = &current.unit_id {
            if current.status != "active" {
                let mut stmt = conn.prepare(
                    "SELECT id FROM cycles
                     WHERE unit_id = ?1 AND status = 'active' AND id != ?2",
                )?;
                let prior_ids: Vec<String> = stmt
                    .query_map(params![uid, id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<_>>()?;
                drop(stmt);
                for prior_id in prior_ids {
                    // Use a direct UPDATE so we bypass the no-todo-residue
                    // gate inside update(): auto-deactivation must always
                    // succeed regardless of the prior cycle's task fanout.
                    conn.execute(
                        "UPDATE cycles SET status = 'completed', ended_at = ?1 WHERE id = ?2",
                        params![now_ms(), prior_id],
                    )?;
                }
            }
        }
    }
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

/// PDD-230: Reject completion when the cycle still has tasks NOT in terminal status.
/// Terminal statuses: done, cancelled, blocked. (`blocked` is treated terminal because
/// it indicates external dependency — the cycle Exit Gate can pass on tracked blockers.)
/// Sentinel phrase "cannot complete cycle:" maps to HTTP 409 in routes/error.rs.
fn assert_no_todo_residue(conn: &Connection, cycle_id: &str) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks
         WHERE cycle_id = ?1 AND status NOT IN ('done', 'cancelled', 'blocked')",
        params![cycle_id],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT COALESCE(ticket_number, id) FROM tasks
         WHERE cycle_id = ?1 AND status NOT IN ('done', 'cancelled', 'blocked')
         ORDER BY idx
         LIMIT 5",
    )?;
    let labels: Vec<String> = stmt
        .query_map(params![cycle_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let suffix = if count > 5 {
        format!(" (+{} more)", count - 5)
    } else {
        String::new()
    };
    bail!(
        "CYCLE_HAS_NON_TERMINAL_TASKS: cannot complete cycle: {} task(s) still non-terminal (todo/in_progress): {}{}",
        count,
        labels.join(", "),
        suffix
    );
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
        unit_id: r.get(2)?,
        idx: r.get(3)?,
        title: r.get(4)?,
        goal: r.get(5)?,
        created_at: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        status: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, tasks, units};

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

    fn setup_with_unit() -> (tempfile::TempDir, Db, String, String, String) {
        let (dir, db, pid) = setup();
        let plan = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &pid,
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
        let cycle = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit.id,
                title: "C1",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        activate(&db.conn, &cycle.id).unwrap();
        (dir, db, plan.id, unit.id, cycle.id)
    }

    fn add_task(db: &mut Db, unit_id: &str, cycle_id: &str, title: &str) -> String {
        let t = tasks::create(
            &mut db.conn,
            tasks::CreateInput {
                unit_id,
                title,
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(cycle_id),
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
        t.id
    }

    #[test]
    fn lifecycle() {
        let (_d, db, pid) = setup();
        let plan = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &pid,
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
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

        let c = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit.id,
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
        assert_eq!(c.unit_id.as_deref(), Some(unit.id.as_str()));

        let active = activate(&db.conn, &c.id).unwrap().unwrap();
        assert_eq!(active.status, "active");
        assert!(active.started_at.is_some());

        let done = complete(&db.conn, &c.id).unwrap().unwrap();
        assert_eq!(done.status, "completed");
        assert!(done.ended_at.is_some());

        let err = activate(&db.conn, &c.id).unwrap_err();
        assert!(err.to_string().contains("cannot be restarted"));
    }

    #[test]
    fn rejects_cycle_without_unit_id() {
        let (_d, db, pid) = setup();
        let err = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: "",
                title: "bad",
                goal: None,
                idx: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("unit_id is required"));
    }

    #[test]
    fn activating_second_cycle_in_unit_auto_completes_prior() {
        // US-CLAWKET-API-CYCLE-005: activating C2 on a unit that already has an
        // active C1 atomically completes C1 instead of rejecting (PDD A8 — same
        // unit cycles are serial, but the daemon collapses the deactivate +
        // activate into one operation).
        let (_d, db, pid) = setup();
        let plan = plans::create(
            &db.conn,
            plans::CreateInput {
                project_id: &pid,
                title: "v1",
                description: None,
                source: None,
                source_path: None,
            },
        )
        .unwrap()
        .unwrap();
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

        let c1 = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit.id,
                title: "C1",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        activate(&db.conn, &c1.id).unwrap();

        let c2 = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit.id,
                title: "C2",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        activate(&db.conn, &c2.id).unwrap().unwrap();

        let c1_after = get(&db.conn, &c1.id).unwrap().unwrap();
        assert_eq!(c1_after.status, "completed");
        let c2_after = get(&db.conn, &c2.id).unwrap().unwrap();
        assert_eq!(c2_after.status, "active");
    }

    #[test]
    fn complete_blocks_with_todo_residue() {
        let (_d, mut db, _plan_id, unit_id, cycle_id) = setup_with_unit();

        let t1 = add_task(&mut db, &unit_id, &cycle_id, "T1");
        let t2 = add_task(&mut db, &unit_id, &cycle_id, "T2");
        let _t3 = add_task(&mut db, &unit_id, &cycle_id, "T3");

        tasks::update(
            &mut db.conn,
            &t1,
            tasks::UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:done".into())),
                ..Default::default()
            },
        )
        .unwrap();
        tasks::update(
            &mut db.conn,
            &t2,
            tasks::UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("test:waiting on external".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let err = complete(&db.conn, &cycle_id).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot complete cycle:"),
            "expected sentinel phrase, got: {msg}"
        );
        // PDD-230: `blocked` is treated terminal (cycle Exit Gate may pass on
        // tracked external blockers). Only T3 (todo) remains as residue.
        assert!(
            msg.contains("1 task(s)"),
            "expected residue count 1, got: {msg}"
        );
    }

    #[test]
    fn completed_cycle_cannot_be_restarted_to_planning() {
        // Lifecycle invariant: status=completed is terminal. Patching back to
        // 'planning' or 'active' must be rejected ("cannot be restarted"); the
        // user has to create a new cycle. Guards `cycles.rs:135-140`.
        let (_d, db, _plan_id, _unit_id, cycle_id) = setup_with_unit();
        complete(&db.conn, &cycle_id).unwrap();

        let err = update(
            &db.conn,
            &cycle_id,
            UpdateFields {
                status: Some("planning".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("cannot be restarted"),
            "expected restart guard, got: {err}"
        );
    }

    #[test]
    fn completed_cycle_cannot_be_restarted_to_active() {
        let (_d, db, _plan_id, _unit_id, cycle_id) = setup_with_unit();
        complete(&db.conn, &cycle_id).unwrap();

        let err = update(
            &db.conn,
            &cycle_id,
            UpdateFields {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("cannot be restarted"),
            "expected restart guard, got: {err}"
        );
    }

    fn project_id_for_unit(conn: &Connection, unit_id: &str) -> String {
        conn.query_row(
            "SELECT p.project_id FROM units u
             JOIN plans p ON p.id = u.plan_id
             WHERE u.id = ?1",
            params![unit_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    }

    #[test]
    fn update_to_active_rejects_when_unit_already_has_active() {
        // PDD A8 — same-unit cycle serialization. `update(status=active)` must
        // refuse when another cycle in the same unit is already active. The
        // public HTTP /cycles/:id/activate path auto-deactivates the prior
        // cycle instead, but a direct PATCH that mutates `status` re-enters
        // this guard (cycles.rs:144-159). The defense must remain even though
        // the activate endpoint usually short-circuits it.
        let (_d, db, _plan_id, unit_id, c1_id) = setup_with_unit();
        let pid = project_id_for_unit(&db.conn, &unit_id);
        // c1 is active. Create c2 in the same unit; it starts as 'planning'.
        // Force c2 to 'active' via update() to verify the guard fires.
        let c2 = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit_id,
                title: "C2",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();

        let err = update(
            &db.conn,
            &c2.id,
            UpdateFields {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("UNIT_HAS_ACTIVE_CYCLE"),
            "expected UNIT_HAS_ACTIVE_CYCLE, got: {err}"
        );
        // C1 must remain active (the guard rejects the update; it does NOT
        // silently swap which cycle is active).
        let c1_after = get(&db.conn, &c1_id).unwrap().unwrap();
        assert_eq!(c1_after.status, "active");
    }

    #[test]
    fn create_with_out_of_order_idx_is_rejected() {
        // API-CYCLE-005: cycles in the same unit must be planned in order.
        // Creating a cycle with idx N when an existing planning cycle has
        // idx > N is `INVALID_REQUEST` (cycles.rs:30-47).
        let (_d, db, _plan_id, unit_id, _c1_id) = setup_with_unit();
        let pid = project_id_for_unit(&db.conn, &unit_id);
        // C1 is active (idx=0). Create C2 (idx=2, planning), then attempt
        // C3 with idx=1 which is below the planning cycle at idx=2.
        create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit_id,
                title: "C2",
                goal: None,
                idx: Some(2),
            },
        )
        .unwrap();

        let err = create(
            &db.conn,
            CreateInput {
                project_id: &pid,
                unit_id: &unit_id,
                title: "C3",
                goal: None,
                idx: Some(1),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("INVALID_REQUEST")
                && err.to_string().contains("planned in order"),
            "expected idx ordering guard, got: {err}"
        );
    }
}
