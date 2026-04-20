use crate::id::{new_id, now_ms};
use crate::models::Task;
use crate::repo::{cycles, plans, units};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

const TERMINAL: &[&str] = &["done", "cancelled"];

pub struct CreateInput<'a> {
    pub unit_id: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub assignee: Option<&'a str>,
    pub idx: Option<i64>,
    pub depends_on: Vec<String>,
    pub parent_task_id: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub complexity: Option<&'a str>,
    pub estimated_edits: Option<i64>,
    pub cycle_id: Option<&'a str>,
    pub reporter: Option<&'a str>,
    pub type_: Option<&'a str>,
}

pub fn create(conn: &mut Connection, input: CreateInput<'_>) -> Result<Option<Task>> {
    if input.unit_id.is_empty() {
        bail!("unit_id is required");
    }

    let unit = units::get(conn, input.unit_id)?;
    if let Some(ref u) = unit {
        if let Some(plan) = plans::get(conn, &u.plan_id)? {
            if plan.status == "draft" {
                bail!(
                    "Cannot create tasks under draft plan \"{}\" ({}). Approve it first: clawket plan approve {}",
                    plan.title, plan.id, plan.id
                );
            }
        }
    }

    let mut cycle_id = input.cycle_id.map(String::from);
    if cycle_id.is_none() {
        if let Some(ref u) = unit {
            if let Some(plan) = plans::get(conn, &u.plan_id)? {
                let actives = cycles::list(
                    conn,
                    cycles::ListFilter {
                        project_id: Some(&plan.project_id),
                        status: Some("active"),
                    },
                )?;
                if actives.len() == 1 {
                    cycle_id = Some(actives[0].id.clone());
                } else if actives.len() > 1 {
                    let ids: Vec<String> = actives.iter().map(|c| c.id.clone()).collect();
                    bail!(
                        "Multiple active cycles found. Specify cycle_id: {}",
                        ids.join(", ")
                    );
                }
            }
        }
    }

    let id = new_id("TASK");
    let ts = now_ms();
    let idx = match input.idx {
        Some(i) => i,
        None => conn.query_row(
            "SELECT COALESCE(MAX(idx), -1) + 1 FROM tasks WHERE unit_id = ?1",
            params![input.unit_id],
            |r| r.get::<_, i64>(0),
        )?,
    };

    let project_key = resolve_project_key(conn, input.unit_id)?;
    let ticket_number = match project_key {
        Some(k) => Some(next_ticket_number(conn, &k)?),
        None => None,
    };

    let body = input.body.unwrap_or("");
    let priority = input.priority.unwrap_or("medium");
    let type_ = input.type_.unwrap_or("task");

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO tasks (id, unit_id, idx, title, body, created_at, status, assignee,
         ticket_number, parent_task_id, priority, complexity, estimated_edits, cycle_id, reporter, type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'todo', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            id,
            input.unit_id,
            idx,
            input.title,
            body,
            ts,
            input.assignee,
            ticket_number,
            input.parent_task_id,
            priority,
            input.complexity,
            input.estimated_edits,
            cycle_id,
            input.reporter,
            type_,
        ],
    )
    .context("insert task")?;

    for dep in &input.depends_on {
        tx.execute(
            "INSERT INTO task_depends_on (task_id, depends_on_task_id) VALUES (?1, ?2)",
            params![id, dep],
        )?;
    }
    tx.commit()?;

    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let canonical = match resolve_id(conn, id)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let task = conn
        .query_row(
            "SELECT id, unit_id, cycle_id, parent_task_id, ticket_number, idx, title, body,
                    priority, complexity, estimated_edits, type, reporter, assignee, agent_id,
                    created_at, started_at, completed_at, status
             FROM tasks WHERE id = ?1",
            params![canonical],
            |r| {
                Ok(Task {
                    id: r.get(0)?,
                    unit_id: r.get(1)?,
                    cycle_id: r.get(2)?,
                    parent_task_id: r.get(3)?,
                    ticket_number: r.get(4)?,
                    idx: r.get(5)?,
                    title: r.get(6)?,
                    body: r.get(7)?,
                    priority: r.get(8)?,
                    complexity: r.get(9)?,
                    estimated_edits: r.get(10)?,
                    type_: r.get(11)?,
                    reporter: r.get(12)?,
                    assignee: r.get(13)?,
                    agent_id: r.get(14)?,
                    created_at: r.get(15)?,
                    started_at: r.get(16)?,
                    completed_at: r.get(17)?,
                    status: r.get(18)?,
                    depends_on: Vec::new(),
                    labels: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut task) = task else {
        return Ok(None);
    };
    task.depends_on = list_dependencies(conn, &canonical)?;
    task.labels = list_labels(conn, &canonical).unwrap_or_default();
    Ok(Some(task))
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub unit_id: Option<&'a str>,
    pub plan_id: Option<&'a str>,
    pub status: Option<&'a str>,
    pub cycle_id: Option<&'a str>,
    pub assignee: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub parent_task_id: Option<Option<&'a str>>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Task>> {
    let mut sql = String::from("SELECT s.id FROM tasks s");
    let mut clauses: Vec<String> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(p) = filter.plan_id {
        sql.push_str(" JOIN units u ON u.id = s.unit_id");
        clauses.push("u.plan_id = ?".into());
        vals.push(p.to_string().into());
    }
    if let Some(u) = filter.unit_id {
        clauses.push("s.unit_id = ?".into());
        vals.push(u.to_string().into());
    }
    if let Some(s) = filter.status {
        clauses.push("s.status = ?".into());
        vals.push(s.to_string().into());
    }
    if let Some(c) = filter.cycle_id {
        clauses.push("s.cycle_id = ?".into());
        vals.push(c.to_string().into());
    }
    if let Some(a) = filter.assignee {
        clauses.push("s.assignee = ?".into());
        vals.push(a.to_string().into());
    }
    if let Some(a) = filter.agent_id {
        clauses.push("s.agent_id = ?".into());
        vals.push(a.to_string().into());
    }
    if let Some(parent) = filter.parent_task_id {
        match parent {
            None => clauses.push("s.parent_task_id IS NULL".into()),
            Some(p) => {
                clauses.push("s.parent_task_id = ?".into());
                vals.push(p.to_string().into());
            }
        }
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY s.unit_id, s.idx");

    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(t) = get(conn, &r?)? {
            out.push(t);
        }
    }
    Ok(out)
}

pub fn append_body(conn: &Connection, id: &str, text: &str) -> Result<Option<Task>> {
    let canonical = match resolve_id(conn, id)? {
        Some(c) => c,
        None => return Ok(None),
    };
    conn.execute(
        "UPDATE tasks SET body = body || ?1 WHERE id = ?2",
        params![text, canonical],
    )?;
    get(conn, &canonical)
}

#[derive(Default, Clone)]
pub struct UpdateFields {
    pub title: Option<String>,
    pub body: Option<Option<String>>,
    pub status: Option<String>,
    pub assignee: Option<Option<String>>,
    pub priority: Option<String>,
    pub complexity: Option<Option<String>>,
    pub estimated_edits: Option<Option<i64>>,
    pub parent_task_id: Option<Option<String>>,
    pub cycle_id: Option<Option<String>>,
    pub unit_id: Option<String>,
    pub reporter: Option<Option<String>>,
    pub type_: Option<String>,
    pub agent_id: Option<Option<String>>,
}

pub fn update(conn: &mut Connection, id: &str, f: UpdateFields) -> Result<Option<Task>> {
    let canonical = resolve_id(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;

    if let Some(status) = &f.status {
        if !matches!(
            status.as_str(),
            "todo" | "in_progress" | "done" | "blocked" | "cancelled"
        ) {
            bail!(
                "Invalid task status: \"{}\". Valid: todo, in_progress, done, blocked, cancelled",
                status
            );
        }
    }

    let mut sets: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();

    fn push_str_opt(
        sets: &mut Vec<&'static str>,
        vals: &mut Vec<rusqlite::types::Value>,
        col: &'static str,
        v: &Option<Option<String>>,
    ) {
        if let Some(inner) = v {
            sets.push(col);
            match inner {
                Some(s) if !s.is_empty() => vals.push(s.clone().into()),
                _ => vals.push(rusqlite::types::Value::Null),
            }
        }
    }

    if let Some(title) = &f.title {
        sets.push("title = ?");
        vals.push(title.clone().into());
    }
    push_str_opt(&mut sets, &mut vals, "body = ?", &f.body);
    if let Some(status) = &f.status {
        sets.push("status = ?");
        vals.push(status.clone().into());
    }
    push_str_opt(&mut sets, &mut vals, "assignee = ?", &f.assignee);
    if let Some(p) = &f.priority {
        sets.push("priority = ?");
        vals.push(p.clone().into());
    }
    push_str_opt(&mut sets, &mut vals, "complexity = ?", &f.complexity);
    if let Some(e) = &f.estimated_edits {
        sets.push("estimated_edits = ?");
        vals.push(match e {
            Some(n) => (*n).into(),
            None => rusqlite::types::Value::Null,
        });
    }
    push_str_opt(
        &mut sets,
        &mut vals,
        "parent_task_id = ?",
        &f.parent_task_id,
    );
    push_str_opt(&mut sets, &mut vals, "cycle_id = ?", &f.cycle_id);
    if let Some(u) = &f.unit_id {
        sets.push("unit_id = ?");
        vals.push(u.clone().into());
    }
    push_str_opt(&mut sets, &mut vals, "reporter = ?", &f.reporter);
    if let Some(t) = &f.type_ {
        sets.push("type = ?");
        vals.push(t.clone().into());
    }
    push_str_opt(&mut sets, &mut vals, "agent_id = ?", &f.agent_id);

    if let Some(status) = &f.status {
        if status == "in_progress" {
            sets.push("started_at = COALESCE(started_at, ?)");
            vals.push(now_ms().into());
        } else if status == "done" || status == "cancelled" {
            sets.push("completed_at = ?");
            vals.push(now_ms().into());
        }
    }

    if sets.is_empty() {
        return get(conn, &canonical);
    }

    vals.push(canonical.clone().into());
    let sql = format!("UPDATE tasks SET {} WHERE id = ?", sets.join(", "));
    let params_iter = rusqlite::params_from_iter(vals.iter());
    conn.execute(&sql, params_iter)?;

    if let Some(status) = &f.status {
        if status == "in_progress" {
            let task = get(conn, &canonical)?
                .ok_or_else(|| anyhow::anyhow!("task vanished after update"))?;
            if let Some(unit) = units::get(conn, &task.unit_id)? {
                if let Some(plan) = plans::get(conn, &unit.plan_id)? {
                    if plan.status != "active" {
                        bail!(
                            "Cannot start task: plan \"{}\" is {}. Approve it first: clawket plan approve {}",
                            plan.title, plan.status, plan.id
                        );
                    }
                }
                if unit.status == "pending" {
                    units::update(
                        conn,
                        &unit.id,
                        units::UpdateFields {
                            status: Some("active".into()),
                            ..Default::default()
                        },
                    )?;
                }
            }
            let Some(cid) = task.cycle_id.as_deref() else {
                bail!(
                    "Cannot start task: no cycle assigned. Assign a cycle first: clawket task update {} --cycle <CYC-ID>",
                    canonical
                );
            };
            if let Some(cycle) = cycles::get(conn, cid)? {
                if cycle.status != "active" {
                    bail!(
                        "Cannot start task: cycle \"{}\" is {}. Activate it first: clawket cycle activate {}",
                        cycle.title, cycle.status, cycle.id
                    );
                }
            }
        }

        if TERMINAL.contains(&status.as_str()) {
            cascade_complete(conn, &canonical)?;
        }
    }

    get(conn, &canonical)
}

pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    let canonical = match resolve_id(conn, id)? {
        Some(c) => c,
        None => return Ok(()),
    };
    conn.execute("DELETE FROM tasks WHERE id = ?1", params![canonical])?;
    Ok(())
}

pub fn add_label(conn: &Connection, id: &str, label: &str) -> Result<Option<Task>> {
    let canonical = resolve_id(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;
    conn.execute(
        "INSERT OR IGNORE INTO task_labels (task_id, label) VALUES (?1, ?2)",
        params![canonical, label],
    )?;
    get(conn, &canonical)
}

pub fn remove_label(conn: &Connection, id: &str, label: &str) -> Result<Option<Task>> {
    let canonical = resolve_id(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;
    conn.execute(
        "DELETE FROM task_labels WHERE task_id = ?1 AND label = ?2",
        params![canonical, label],
    )?;
    get(conn, &canonical)
}

fn cascade_complete(conn: &mut Connection, task_id: &str) -> Result<()> {
    let task = match get(conn, task_id)? {
        Some(t) => t,
        None => return Ok(()),
    };

    let unit_tasks = list(
        conn,
        ListFilter {
            unit_id: Some(&task.unit_id),
            ..Default::default()
        },
    )?;
    let all_unit_done = !unit_tasks.is_empty()
        && unit_tasks
            .iter()
            .all(|t| TERMINAL.contains(&t.status.as_str()));
    if all_unit_done {
        if let Some(unit) = units::get(conn, &task.unit_id)? {
            if unit.status != "completed" {
                units::update(
                    conn,
                    &unit.id,
                    units::UpdateFields {
                        status: Some("completed".into()),
                        ..Default::default()
                    },
                )?;
                let plan_units = units::list(
                    conn,
                    units::ListFilter {
                        plan_id: Some(&unit.plan_id),
                        ..Default::default()
                    },
                )?;
                if !plan_units.is_empty() && plan_units.iter().all(|u| u.status == "completed") {
                    if let Some(plan) = plans::get(conn, &unit.plan_id)? {
                        if plan.status == "active" {
                            plans::update(
                                conn,
                                &plan.id,
                                plans::UpdateFields {
                                    status: Some("completed".into()),
                                    ..Default::default()
                                },
                            )?;
                        }
                    }
                }
            }
        }
    }

    if let Some(cid) = task.cycle_id.as_deref() {
        let cycle_tasks = list(
            conn,
            ListFilter {
                cycle_id: Some(cid),
                ..Default::default()
            },
        )?;
        let all_cycle_done = !cycle_tasks.is_empty()
            && cycle_tasks
                .iter()
                .all(|t| TERMINAL.contains(&t.status.as_str()));
        if all_cycle_done {
            if let Some(cycle) = cycles::get(conn, cid)? {
                if cycle.status == "active" {
                    cycles::update(
                        conn,
                        &cycle.id,
                        cycles::UpdateFields {
                            status: Some("completed".into()),
                            ..Default::default()
                        },
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn resolve_id(conn: &Connection, id: &str) -> Result<Option<String>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT id FROM tasks WHERE id = ?1 OR ticket_number = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row)
}

fn resolve_project_key(conn: &Connection, unit_id: &str) -> Result<Option<String>> {
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT p.key FROM projects p
             JOIN plans pl ON pl.project_id = p.id
             JOIN units u ON u.plan_id = pl.id
             WHERE u.id = ?1",
            params![unit_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row.flatten())
}

fn next_ticket_number(conn: &Connection, project_key: &str) -> Result<String> {
    let prefix = format!("{}-", project_key);
    let last: Option<String> = conn
        .query_row(
            "SELECT ticket_number FROM tasks
             WHERE ticket_number LIKE ?1 || '%'
             ORDER BY CAST(SUBSTR(ticket_number, LENGTH(?1) + 1) AS INTEGER) DESC
             LIMIT 1",
            params![prefix],
            |r| r.get(0),
        )
        .optional()?;
    let next_num = match last {
        None => 1,
        Some(t) => {
            let n: i64 = t.trim_start_matches(&prefix).parse().unwrap_or(0);
            n + 1
        }
    };
    Ok(format!("{}{}", prefix, next_num))
}

fn list_dependencies(conn: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT depends_on_task_id FROM task_depends_on WHERE task_id = ?1",
    )?;
    let rows = stmt.query_map(params![task_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn list_labels(conn: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT label FROM task_labels WHERE task_id = ?1 ORDER BY label")?;
    let rows = stmt.query_map(params![task_id], |r| r.get::<_, String>(0))?;
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
    use crate::repo::{cycles, plans, projects, units};

    struct Scene {
        _dir: tempfile::TempDir,
        db: Db,
        plan_id: String,
        unit_id: String,
        cycle_id: String,
    }

    fn setup(approve: bool) -> Scene {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.sqlite")).unwrap();
        let project = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "TradingBot",
                description: None,
                cwd: None,
                key: Some("TB"),
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
        if approve {
            plans::approve(&db.conn, &plan.id).unwrap();
        }
        let unit = units::create(
            &db.conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: "U1",
                goal: None,
                idx: None,
                approval_required: false,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        let cycle = cycles::create(
            &db.conn,
            cycles::CreateInput {
                project_id: &project.id,
                title: "C1",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        Scene {
            _dir: dir,
            db,
            plan_id: plan.id,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    #[test]
    fn rejects_tasks_under_draft_plan() {
        let mut s = setup(false);
        let err = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
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
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("draft plan"));
    }

    #[test]
    fn create_and_ticket_number() {
        let mut s = setup(true);
        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "T1",
                body: Some("hi"),
                assignee: Some("main"),
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(&s.cycle_id),
                reporter: None,
                type_: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(t.ticket_number.as_deref(), Some("TB-1"));
        assert_eq!(t.status, "todo");
        assert_eq!(t.cycle_id.as_deref(), Some(s.cycle_id.as_str()));

        let t2 = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "T2",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![t.id.clone()],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(&s.cycle_id),
                reporter: None,
                type_: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(t2.ticket_number.as_deref(), Some("TB-2"));
        assert_eq!(t2.depends_on, vec![t.id.clone()]);

        let by_ticket = get(&s.db.conn, "TB-1").unwrap().unwrap();
        assert_eq!(by_ticket.id, t.id);
    }

    #[test]
    fn state_machine_requires_active_cycle() {
        let mut s = setup(true);
        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "T1",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(&s.cycle_id),
                reporter: None,
                type_: None,
            },
        )
        .unwrap()
        .unwrap();

        let err = update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("in_progress".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"));

        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let started = update(
            &mut s.db.conn,
            "TB-1",
            UpdateFields {
                status: Some("in_progress".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(started.status, "in_progress");
        assert!(started.started_at.is_some());

        let unit = units::get(&s.db.conn, &s.unit_id).unwrap().unwrap();
        assert_eq!(unit.status, "active");
    }

    #[test]
    fn cascade_completes_unit_plan_cycle() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "only",
                body: None,
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(&s.cycle_id),
                reporter: None,
                type_: None,
            },
        )
        .unwrap()
        .unwrap();

        update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let unit = units::get(&s.db.conn, &s.unit_id).unwrap().unwrap();
        assert_eq!(unit.status, "completed");
        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(plan.status, "completed");
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(cycle.status, "completed");
    }

    #[test]
    fn labels_and_append_body() {
        let mut s = setup(true);
        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "L",
                body: Some("start"),
                assignee: None,
                idx: None,
                depends_on: vec![],
                parent_task_id: None,
                priority: None,
                complexity: None,
                estimated_edits: None,
                cycle_id: Some(&s.cycle_id),
                reporter: None,
                type_: None,
            },
        )
        .unwrap()
        .unwrap();

        add_label(&s.db.conn, &t.id, "urgent").unwrap();
        add_label(&s.db.conn, &t.id, "backend").unwrap();
        let with_labels = get(&s.db.conn, &t.id).unwrap().unwrap();
        assert_eq!(with_labels.labels, vec!["backend", "urgent"]);

        remove_label(&s.db.conn, &t.id, "urgent").unwrap();
        let after = get(&s.db.conn, &t.id).unwrap().unwrap();
        assert_eq!(after.labels, vec!["backend"]);

        append_body(&s.db.conn, &t.id, "\nmore").unwrap();
        let appended = get(&s.db.conn, &t.id).unwrap().unwrap();
        assert_eq!(appended.body, "start\nmore");
    }
}
