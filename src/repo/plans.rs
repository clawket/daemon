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
    /// Migration 027: opt-in Stop-hook auto-advance flag (default false).
    pub auto_advance: bool,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Plan>> {
    let id = new_id("PLAN");
    let ts = now_ms();
    let source = input.source.unwrap_or("manual");
    conn.execute(
        "INSERT INTO plans (id, project_id, title, description, source, source_path, created_at, status, auto_advance)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'draft', ?8)",
        params![id, input.project_id, input.title, input.description, source, input.source_path, ts, input.auto_advance as i64],
    )
    .context("insert plan")?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Plan>> {
    let plan = conn
        .query_row(
            "SELECT id, project_id, title, description, source, source_path, created_at, approved_at, status, auto_advance
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
        "SELECT id, project_id, title, description, source, source_path, created_at, approved_at, status, auto_advance FROM plans",
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
    /// Migration 027: toggle opt-in Stop-hook auto-advance.
    pub auto_advance: Option<bool>,
}

/// How much of this plan's own work is still outstanding — the single
/// definition every completion gate asks (LM-11093).
///
/// Two exclusions and one carve-out:
///   `blocked`          — waiting on something outside this plan, so holding the
///                        plan open cannot resolve it. Matches
///                        `repo::cycles::assert_no_todo_residue` (PDD-230),
///                        which has always read it this way; the disagreement
///                        let a cycle complete while its own plan refused,
///                        citing a task that cycle had already accepted.
///   `cycle_id IS NULL` — backlog: deferred via `task update --cycle ""`.
///                        `tasks.unit_id` is NOT NULL, so a detached task keeps
///                        its unit and this JOIN still reaches it.
///   `blocked` + `qa_status = 'defect'`
///                      — NOT excluded. `routes::discover::bulk_sync` transcribes
///                        a QA defect as `blocked`, and a defect is work inside
///                        the plan, so it still counts. See the body comment.
///
/// Both the repo gate below and the route gate in `routes::plans` call this, so
/// the two cannot drift — they used to hold separate copies of the predicate,
/// and a test could pin one while the other silently regressed. The Rust-side
/// twin is `repo::tasks::container_terminal`, which `cascade_complete` applies to
/// the same criterion in memory (`container_terminal` wraps the terminal-status
/// set with the defect carve-out). Change one and the other must move.
pub fn count_completion_residue(conn: &Connection, plan_id: &str) -> Result<i64> {
    let n: i64 = conn.query_row(
        // The `qa_status` clause is the SQL half of `repo::tasks::container_terminal`:
        // a QA defect is transcribed as `status = 'blocked'` but is work inside the
        // plan, so it still counts as residue. Without it this gate would pass a
        // plan the cascade now refuses to complete — the two-paths-disagree defect
        // this function exists to prevent. It is `blocked` AND `defect` for the
        // same reason as the Rust twin: a stale verdict on a `cancelled` row must
        // not pin the plan open forever.
        "SELECT COUNT(*) FROM tasks t
         JOIN units u ON t.unit_id = u.id
         WHERE u.plan_id = ?1 AND t.cycle_id IS NOT NULL
           AND (t.status NOT IN ('done', 'cancelled', 'blocked')
                OR (t.status = 'blocked' AND t.qa_status = 'defect'))",
        rusqlite::params![plan_id],
        |r| r.get(0),
    )?;
    Ok(n)
}

pub fn update(conn: &Connection, id: &str, f: UpdateFields) -> Result<Option<Plan>> {
    if let Some(status) = &f.status {
        if !matches!(status.as_str(), "draft" | "active" | "completed") {
            bail!(
                "Invalid plan status: \"{}\". Valid: draft, active, completed",
                status
            );
        }

        // FIX-DAEMON-003: completion residue gate
        if status == "completed" {
            let pending_tasks = count_completion_residue(conn, id)?;
            if pending_tasks > 0 {
                // Name the real criterion. A blocked or backlog task no longer
                // appears here, so listing only done/cancelled would send the user
                // looking for tasks the gate already accepted — and the count now
                // also includes open QA defects, which ARE spelled `blocked`, so
                // saying only "todo/in_progress" would hide them the other way.
                bail!(
                    "Cannot complete plan: {} scheduled task(s) still open (todo/in_progress, or an unresolved QA defect)",
                    pending_tasks
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
    if let Some(desc) = f.description {
        sets.push("description = ?");
        vals.push(match desc {
            Some(s) => s.into(),
            None => rusqlite::types::Value::Null,
        });
    }
    if let Some(aa) = f.auto_advance {
        sets.push("auto_advance = ?");
        vals.push((aa as i64).into());
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

/// Migration 027 / continuation endpoint: the next actionable step for a plan.
///
/// Used by the Stop hook (via `GET /continuation`) to decide whether the agent
/// should keep going. The walk is strictly idx-ordered so it mirrors the
/// sequential execution contract (Unit.idx then Task.idx):
///
/// 1. Walk units by ascending idx. For the first unit that still has a `todo`
///    task, return that task (lowest task idx wins) → `NextStep::Task`.
/// 2. If every unit up to and including unit N is fully terminal (all tasks
///    done/cancelled, or the unit has no tasks at all) but a later unit N+1
///    has no tasks yet, return that next unit as a phase to start →
///    `NextStep::Unit`. A unit with non-terminal tasks (blocked / in_progress
///    but no todo) is NOT skipped past — the agent owes work there, so we stop
///    advancing rather than jumping ahead.
/// 3. Otherwise `None` — the plan has no actionable next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    Task { id: String, title: String },
    Unit { id: String, title: String },
}

pub fn next_actionable(conn: &Connection, plan_id: &str) -> Result<Option<NextStep>> {
    use crate::repo::{tasks, units};

    let all_units = units::list(
        conn,
        units::ListFilter {
            plan_id: Some(plan_id),
        },
    )?;
    // units::list already orders by (plan_id, idx); keep that contract explicit.

    for u in &all_units {
        let unit_tasks = tasks::list(
            conn,
            tasks::ListFilter {
                unit_id: Some(&u.id),
                ..Default::default()
            },
        )?;
        // tasks::list orders by (unit_id, idx) so the first todo is the lowest idx.
        if let Some(todo) = unit_tasks.iter().find(|t| t.status == "todo") {
            return Ok(Some(NextStep::Task {
                id: todo.id.clone(),
                title: todo.title.clone(),
            }));
        }
        // No todo in this unit. If it still carries non-terminal work
        // (in_progress / blocked), the agent must resolve it here — stop the
        // walk instead of jumping to a later phase.
        let has_non_terminal = unit_tasks
            .iter()
            .any(|t| !matches!(t.status.as_str(), "done" | "cancelled"));
        if has_non_terminal {
            return Ok(None);
        }
        // This unit is fully terminal (or empty); continue to the next unit.
    }

    // Every unit walked is fully terminal. If a unit has no tasks at all it is
    // a not-yet-started phase — surface the first such unit so the agent can
    // create + run its tasks. (Walk again to find the earliest empty unit.)
    for u in &all_units {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE unit_id = ?1",
            params![u.id],
            |r| r.get(0),
        )?;
        if count == 0 {
            return Ok(Some(NextStep::Unit {
                id: u.id.clone(),
                title: u.title.clone(),
            }));
        }
    }

    Ok(None)
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
        auto_advance: r.get::<_, i64>(9)? != 0,
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
                auto_advance: false,
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

    // ----- Migration 027: auto_advance + next_actionable -----

    use crate::repo::{tasks, units};

    fn make_plan(db: &mut Db, pid: &str, auto_advance: bool) -> String {
        create(
            &db.conn,
            CreateInput {
                project_id: pid,
                title: "P1",
                description: None,
                source: None,
                source_path: None,
                auto_advance,
            },
        )
        .unwrap()
        .unwrap()
        .id
    }

    fn make_unit(db: &mut Db, plan_id: &str, title: &str) -> String {
        units::create(
            &db.conn,
            units::CreateInput {
                plan_id,
                title,
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap()
        .id
    }

    fn make_task(db: &mut Db, unit_id: &str, title: &str) -> String {
        tasks::create(
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
        .unwrap()
        .id
    }

    fn set_status(db: &Db, task_id: &str, status: &str) {
        db.conn
            .execute(
                "UPDATE tasks SET status = ?1 WHERE id = ?2",
                params![status, task_id],
            )
            .unwrap();
    }

    /// `make_task` builds backlog tasks (`cycle_id: None`); these tests need
    /// scheduled ones. `tasks.cycle_id` has a FK, so the cycle has to be real.
    fn make_cycle(db: &Db, project_id: &str, unit_id: &str) -> String {
        crate::repo::cycles::create(
            &db.conn,
            crate::repo::cycles::CreateInput {
                project_id,
                unit_id,
                title: "C1",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap()
        .id
    }

    fn schedule(db: &Db, task_id: &str, cycle_id: &str) {
        db.conn
            .execute(
                "UPDATE tasks SET cycle_id = ?1 WHERE id = ?2",
                params![cycle_id, task_id],
            )
            .unwrap();
    }

    // LM-11093: `count_completion_residue` is the single definition both the
    // repo gate (below) and the route gate (`routes::plans`) ask, so this pins
    // the predicate for both. Before they shared a function each held its own
    // copy, and a test could pin one while the other silently regressed — which
    // is exactly what happened: the repo copy was covered, the HTTP path users
    // actually reach was not.
    #[test]
    fn completion_residue_counts_only_unfinished_scheduled_work() {
        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);
        let plan_id = make_plan(&mut db, &pid, false);
        let unit_id = make_unit(&mut db, &plan_id, "U1");
        let cyc = make_cycle(&db, &pid, &unit_id);

        // Residue: scheduled and genuinely unfinished.
        let todo = make_task(&mut db, &unit_id, "todo");
        schedule(&db, &todo, &cyc);
        let running = make_task(&mut db, &unit_id, "running");
        schedule(&db, &running, &cyc);
        set_status(&db, &running, "in_progress");
        assert_eq!(count_completion_residue(&db.conn, &plan_id).unwrap(), 2);

        // Not residue: finished, abandoned, or waiting on something outside.
        for (title, status) in [
            ("shipped", "done"),
            ("dropped", "cancelled"),
            ("waiting", "blocked"),
        ] {
            let id = make_task(&mut db, &unit_id, title);
            schedule(&db, &id, &cyc);
            set_status(&db, &id, status);
        }
        assert_eq!(
            count_completion_residue(&db.conn, &plan_id).unwrap(),
            2,
            "done/cancelled/blocked are not this plan's remaining work"
        );

        // Not residue: deferred to a later round, whatever its status.
        let deferred = make_task(&mut db, &unit_id, "deferred");
        assert_eq!(
            count_completion_residue(&db.conn, &plan_id).unwrap(),
            2,
            "a backlog task is deferred work, not remaining work"
        );

        // Scheduling that same task makes it count — the exclusion is keyed on
        // attachment, not on the task.
        schedule(&db, &deferred, &cyc);
        assert_eq!(
            count_completion_residue(&db.conn, &plan_id).unwrap(),
            3,
            "re-attaching returns the task to the plan's remaining work"
        );

        // Clearing the last two leaves the plan completable.
        for id in [&todo, &running, &deferred] {
            set_status(&db, id, "done");
        }
        assert_eq!(count_completion_residue(&db.conn, &plan_id).unwrap(), 0);
    }

    // LM-11093: the QA round tally. `/discover/*` and the dashboard's
    // Discover-Loop panel both call this, so pinning it here covers both — the
    // panel used to re-implement the tally and disagreed on two counts at once
    // (it included backlog, and read `qa_status` alone, missing the
    // DOGFOOD-039 two-signal defect). Lives in this module rather than
    // `routes::discover` because that file has no test scaffolding.
    #[test]
    fn qa_round_counts_exclude_backlog_and_read_both_signals() {
        use crate::routes::discover::query_plan_task_counts;

        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);
        let plan_id = make_plan(&mut db, &pid, false);
        let unit_id = make_unit(&mut db, &plan_id, "U1");
        let cyc = make_cycle(&db, &pid, &unit_id);

        let set_qa = |db: &Db, id: &str, qa: &str| {
            db.conn
                .execute(
                    "UPDATE tasks SET qa_status = ?1 WHERE id = ?2",
                    params![qa, id],
                )
                .unwrap();
        };

        // Explicit qa_status on scheduled tasks.
        let ok = make_task(&mut db, &unit_id, "ok");
        schedule(&db, &ok, &cyc);
        set_qa(&db, &ok, "pass");
        let bad = make_task(&mut db, &unit_id, "bad");
        schedule(&db, &bad, &cyc);
        set_qa(&db, &bad, "defect");

        // DOGFOOD-039: a scheduled task with no qa_status still signals through
        // `status`. Dropping this arm is the divergence the dashboard had.
        let implicit = make_task(&mut db, &unit_id, "implicit-defect");
        schedule(&db, &implicit, &cyc);
        set_status(&db, &implicit, "blocked");

        let c = query_plan_task_counts(&db.conn, &plan_id).unwrap();
        assert_eq!(c.pass, 1);
        assert_eq!(c.defect, 2, "qa_status=defect plus the blocked-without-qa");
        assert_eq!(c.total, 3);

        // A deferred defect must not count against this round — it is not in it.
        let deferred = make_task(&mut db, &unit_id, "deferred-defect");
        set_qa(&db, &deferred, "defect");
        let c = query_plan_task_counts(&db.conn, &plan_id).unwrap();
        assert_eq!(
            c.defect, 2,
            "a backlog task belongs to a later round, not this one"
        );
        assert_eq!(c.total, 3, "and it is not part of this round's denominator");
    }

    #[test]
    fn auto_advance_round_trips() {
        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);

        let plan_id = make_plan(&mut db, &pid, true);
        assert!(get(&db.conn, &plan_id).unwrap().unwrap().auto_advance);

        // Toggle off via update.
        update(
            &db.conn,
            &plan_id,
            UpdateFields {
                auto_advance: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!get(&db.conn, &plan_id).unwrap().unwrap().auto_advance);

        // Default (omitted) is false.
        let plan2 = make_plan(&mut db, &pid, false);
        assert!(!get(&db.conn, &plan2).unwrap().unwrap().auto_advance);
    }

    #[test]
    fn next_actionable_task_then_unit_then_none() {
        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);
        let plan_id = make_plan(&mut db, &pid, true);
        // Tasks can be created under a draft plan as todo; approval isn't
        // required for next_actionable (a read-only walk).
        let u1 = make_unit(&mut db, &plan_id, "U1");
        let u2 = make_unit(&mut db, &plan_id, "U2"); // empty phase

        let t1 = make_task(&mut db, &u1, "T1");
        let t2 = make_task(&mut db, &u1, "T2");

        // (1) First todo task in idx order.
        match next_actionable(&db.conn, &plan_id).unwrap() {
            Some(NextStep::Task { id, title }) => {
                assert_eq!(id, t1);
                assert_eq!(title, "T1");
            }
            other => panic!("expected T1 task, got {other:?}"),
        }

        // (2) After T1 done, T2 is next.
        set_status(&db, &t1, "done");
        match next_actionable(&db.conn, &plan_id).unwrap() {
            Some(NextStep::Task { id, .. }) => assert_eq!(id, t2),
            other => panic!("expected T2 task, got {other:?}"),
        }

        // (3) U1 fully terminal, U2 empty → next is the U2 phase.
        set_status(&db, &t2, "done");
        match next_actionable(&db.conn, &plan_id).unwrap() {
            Some(NextStep::Unit { id, title }) => {
                assert_eq!(id, u2);
                assert_eq!(title, "U2");
            }
            other => panic!("expected U2 unit, got {other:?}"),
        }

        // (4) Give U2 a task and complete it → nothing actionable.
        let t3 = make_task(&mut db, &u2, "T3");
        set_status(&db, &t3, "cancelled");
        assert_eq!(next_actionable(&db.conn, &plan_id).unwrap(), None);
    }

    #[test]
    fn next_actionable_stops_on_non_terminal_unit() {
        // A unit with an in_progress (non-todo, non-terminal) task must not be
        // skipped to advance to a later empty phase — the agent owes work here.
        let (_d, mut db) = tmp_db();
        let pid = make_project(&mut db);
        let plan_id = make_plan(&mut db, &pid, true);
        let u1 = make_unit(&mut db, &plan_id, "U1");
        let _u2 = make_unit(&mut db, &plan_id, "U2"); // empty later phase

        let t1 = make_task(&mut db, &u1, "T1");
        set_status(&db, &t1, "in_progress");

        // No todo anywhere, U1 has non-terminal work → None (do not jump to U2).
        assert_eq!(next_actionable(&db.conn, &plan_id).unwrap(), None);
    }
}
