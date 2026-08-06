use crate::id::{new_id, now_ms};
use crate::models::Task;
use crate::repo::{cycles, knowledge, plans, units};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// LM-11093: statuses that stop a task from holding its **container** open.
///
/// The set is `done | cancelled | blocked` — one wider than a task's own
/// terminal states, by `blocked`. A blocked task waits on an external
/// dependency, so no amount of work inside the container can move it — holding
/// the container open does not make the blocker resolve, it just makes the
/// container permanently uncompletable, since `blocked` has no path to `done`
/// from inside. `repo::cycles::assert_no_todo_residue` has read `blocked` this
/// way since PDD-230 ("the cycle Exit Gate can pass on tracked blockers"); the
/// plan gates disagreed, so a cycle could complete while its own plan refused
/// to, naming a task the cycle had already accepted.
///
/// Both cascade arms use it, and they must move together: "a completed plan has
/// no active cycle" is asserted independently by `routes::plans`
/// (`PLAN_HAS_ACTIVE_CYCLES`, PDD-231) and `routes::discover` (DOGFOOD-004).
/// If the cycle arm lagged the plan arm, the cascade path would produce a state
/// the route path rejects.
///
/// NOT used for "what should I work on next": `plans::next_actionable` still
/// stops at a blocked task, because the agent does owe attention there.
/// Container completion and next-step routing ask different questions of the
/// same status.
///
/// The SQL twin of this set is `repo::plans::count_completion_residue`
/// (`NOT IN ('done','cancelled','blocked')`), which both plan gates call, plus
/// the cycle gate's two copies in `repo::cycles::assert_no_todo_residue`. Keep
/// them in lock-step — and note `container_terminal()` below, not this bare set,
/// is what the cascade asks.
const CONTAINER_TERMINAL: &[&str] = &["done", "cancelled", "blocked"];

/// Does this task still hold its containers open?
///
/// Status alone cannot answer, because `blocked` carries two meanings in this
/// codebase. The set above assumes the first: waiting on something outside the
/// plan, unresolvable from inside, so holding the plan open buys nothing. But
/// `routes::discover::bulk_sync` transcribes a QA **defect** as `blocked`
/// (`"defect" => "blocked"`), and a defect is work *inside* the plan. Reading
/// status alone, a round of 9 pass + 1 defect is entirely `{done, blocked}` and
/// the cascade closes plan and cycle over an open defect — after which the
/// `PLAN_COMPLETED` freeze rejects further `create` and the plan-active guard
/// rejects `in_progress`, so nobody can fix the defect without reopening by hand.
///
/// `qa_status` already distinguishes them: bulk_sync writes `'defect'` alongside
/// the status. So the two meanings are separable without changing the
/// transcription, which the QA tallies and the dashboard depend on
/// (`routes::discover::query_plan_task_counts` keys defect detection on
/// `status = 'blocked'`).
///
/// `qa_status` is `None` for every non-QA task, so ordinary blockers keep the
/// PDD-230 behaviour unchanged.
///
/// The check is `blocked` AND `defect`, not `defect` alone: `qa_status` is not
/// cleared by a status change (`update` writes it only when the patch carries
/// it) and `blocked → cancelled` is a legal transition, so a cancelled row can
/// still hold a stale `defect` verdict. Keyed on the verdict alone, that row
/// would count as open work forever and the only way out of `cancelled` is back
/// to `todo` — the permanently-uncompletable plan this whole change set exists
/// to remove, reintroduced on a new axis. `cancelled` is unambiguous in a way
/// `blocked` is not: it means the work will not be done, whatever a stale QA
/// verdict says.
fn container_terminal(status: &str, qa_status: Option<&str>) -> bool {
    if status == "blocked" && qa_status == Some("defect") {
        return false;
    }
    CONTAINER_TERMINAL.contains(&status)
}

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
    /// LM-263 — explicit `atomic_size_hint` from the strict envelope.
    pub atomic_size_hint: Option<&'a str>,
    /// LM-263 — explicit `decomposition_policy` from the strict envelope.
    pub decomposition_policy: Option<&'a str>,
    // FIX-DAEMON-001: tier
    pub tier: Option<&'a str>,
    // FIX-DAEMON-006: QA workflow fields
    pub qa_status: Option<&'a str>,
    pub scenario_id: Option<&'a str>,
    pub defect_task: Option<&'a str>,
    pub scenario_amendment: Option<&'a str>,
    // US-CKT-SCHEMA-011: evidence (file:line or reasoning summary)
    pub evidence: Option<&'a str>,
    // US-CKT-SCHEMA-021: batch_id (ULID of sub-agent batch invocation)
    pub batch_id: Option<&'a str>,
}

pub fn create(conn: &mut Connection, input: CreateInput<'_>) -> Result<Option<Task>> {
    let tx = conn.transaction()?;
    let id = create_in_tx(&tx, input)?;
    tx.commit()?;
    get(conn, &id)
}

/// Tx-aware variant of [`create`]: runs all validation + the INSERT inside the
/// caller-owned transaction and returns the new task id WITHOUT committing.
/// Lets a route handler wrap `create_in_tx` + `sign_envelope_in_tx` in one
/// transaction so a rejected envelope leaves no orphan task (D2 atomicity).
pub fn create_in_tx(conn: &Transaction, input: CreateInput<'_>) -> Result<String> {
    if input.unit_id.is_empty() {
        bail!("unit_id is required");
    }

    if input.title.trim().is_empty() {
        bail!("INVALID_TITLE: title cannot be empty");
    }

    // FIX-DAEMON-r2-tier: validate tier if explicitly provided. Empty/None falls
    // through and the SQL DEFAULT (then the application fallback below) supplies 'med'.
    if let Some(t) = input.tier {
        if !t.is_empty() && !matches!(t, "low" | "med" | "high") {
            bail!(
                "INVALID_TIER: tier='{}' is not allowed. Valid: low, med, high",
                t
            );
        }
    }
    // FIX-DAEMON-r2-qa: validate qa_status if explicitly provided.
    if let Some(q) = input.qa_status {
        if !q.is_empty() && !matches!(q, "pass" | "defect" | "scenario_error") {
            bail!(
                "INVALID_QA_STATUS: qa_status='{}' is not allowed. Valid: pass, defect, scenario_error",
                q
            );
        }
    }

    if let Some(parent_id) = input.parent_task_id {
        if !parent_id.is_empty() {
            let parent_canonical = resolve_id(conn, parent_id)?
                .ok_or_else(|| anyhow::anyhow!("parent_task_id not found: {}", parent_id))?;
            // The new task has no id yet, so a cycle is impossible *via* it,
            // but we still ensure the parent chain itself is acyclic so we
            // never seed corruption from the daemon side.
            if has_existing_cycle(conn, &parent_canonical)? {
                bail!(
                    "parent_task_id chain has an existing cycle through {}",
                    parent_canonical
                );
            }
        }
    }

    let unit = units::get(conn, input.unit_id)?;

    // LM-11031: completed plans are structurally frozen — block new task
    // creation under any unit whose parent plan is completed. Pairs with the
    // symmetric gate in units::create and the residue gate at
    // PATCH /plans/:id, so the only way to add work under a completed plan
    // is to re-open it (PATCH status → active) or create a follow-up plan.
    // Without this gate a delayed `task create` after cascade-complete would
    // silently produce orphan TODO tasks under a Complete plan (LM-11031
    // root cause: the cascade-up logic at line ~1049 commits the plan to
    // 'completed' once all unit tasks are terminal, but nothing prevented a
    // subsequent create from re-introducing non-terminal work).
    if let Some(ref u) = unit {
        if let Some(plan) = plans::get(conn, &u.plan_id)? {
            if plan.status == "completed" {
                bail!(
                    "PLAN_COMPLETED: cannot create task under completed plan '{}' (via unit '{}'). Re-open the plan (PATCH status → active) or create a follow-up plan.",
                    plan.id,
                    u.id
                );
            }
        }
    }

    // #9: cycle_id is taken verbatim from the caller — no auto-infer from the
    // project's active cycle. The public create/subtask routes enforce
    // API-TASK-001 (cycle_id required) at the HTTP boundary; the import path
    // intentionally creates cycle-less backlog tasks (cycle assigned later).
    // The previous auto-infer here silently bound such backlog/subtask rows to
    // whatever single cycle happened to be active project-wide — an unrelated
    // binding — and was the dead-policy remnant that caused the cycle-gate
    // confusion (#9). A NULL cycle_id now stays NULL.
    let cycle_id = input.cycle_id.map(String::from);

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

    // FIX-DAEMON-r2-cycle-unit: enforce PDD A4 — cycle.unit_id must match task.unit_id.
    if let Some(ref cid) = cycle_id {
        if let Some(cycle) = cycles::get(conn, cid)? {
            if let Some(ref cuid) = cycle.unit_id {
                if cuid != input.unit_id {
                    bail!(
                        "CYCLE_UNIT_MISMATCH: cycle '{}' is bound to unit '{}' but task is for unit '{}' (PDD A4: Cycle ⊂ Unit)",
                        cid,
                        cuid,
                        input.unit_id
                    );
                }
            }
        }
    }

    let body = input.body.unwrap_or("");
    let priority = input.priority.unwrap_or("medium");
    let type_ = input.type_.unwrap_or("task");
    let atomic_size_hint = input.atomic_size_hint.unwrap_or("small");
    let decomposition_policy = input.decomposition_policy.unwrap_or("auto");
    // FIX-DAEMON-r2-tier: default tier='med' when caller didn't supply one.
    let tier_resolved: &str = match input.tier {
        Some(t) if !t.is_empty() => t,
        _ => "med",
    };

    conn.execute(
        "INSERT INTO tasks (id, unit_id, idx, title, body, created_at, status, assignee,
         ticket_number, parent_task_id, priority, complexity, estimated_edits, cycle_id, reporter, type,
         atomic_size_hint, decomposition_policy, tier, qa_status, scenario_id, defect_task, scenario_amendment,
         evidence, batch_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'todo', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
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
            atomic_size_hint,
            decomposition_policy,
            tier_resolved,
            input.qa_status,
            input.scenario_id,
            input.defect_task,
            input.scenario_amendment,
            input.evidence,
            input.batch_id,
        ],
    )
    .context("insert task")?;

    for dep in &input.depends_on {
        conn.execute(
            "INSERT INTO task_depends_on (task_id, depends_on_task_id) VALUES (?1, ?2)",
            params![id, dep],
        )?;
    }

    // US-CLAWKET-TIER-044: round-aware tier escalation. When a task is being
    // created in a round-N cycle (round = ordinal of the cycle within its
    // unit, by created_at), and the same scenario_id had qa_status='defect'
    // in round N-1 of the same unit, auto-set escalation_reason to
    // 'prior-round-defect'. Skipped silently when any of the prerequisites
    // (cycle, unit, scenario_id, prior round) is missing.
    if let (Some(cid), Some(sid)) = (cycle_id.as_deref(), input.scenario_id) {
        if !sid.is_empty() {
            // Find round number = position of `cid` in the unit's cycle list
            // ordered by created_at ASC (1-indexed). The unit_id comes from
            // the task being inserted (input.unit_id).
            let prior_cycle_id: Option<String> = conn
                .query_row(
                    "SELECT prev.id
                 FROM cycles cur
                 JOIN cycles prev ON prev.unit_id = cur.unit_id
                 WHERE cur.id = ?1
                   AND prev.unit_id = ?2
                   AND prev.created_at < cur.created_at
                 ORDER BY prev.created_at DESC, prev.id DESC
                 LIMIT 1",
                    params![cid, input.unit_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            if let Some(prev_cid) = prior_cycle_id {
                let prior_defect_exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM tasks
                         WHERE cycle_id = ?1 AND scenario_id = ?2 AND qa_status = 'defect'
                         LIMIT 1",
                        params![prev_cid, sid],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .is_some();
                if prior_defect_exists {
                    conn.execute(
                        "UPDATE tasks SET escalation_reason = 'prior-round-defect' WHERE id = ?1",
                        params![id],
                    )?;
                }
            }
        }
    }

    Ok(id)
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
                    created_at, started_at, completed_at, status, active_envelope_id,
                    atomic_size_hint, decomposition_policy, tier,
                    qa_status, scenario_id, defect_task, scenario_amendment,
                    tier_used, escalation_reason,
                    evidence, batch_id
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
                    active_envelope_id: r.get(19)?,
                    atomic_size_hint: r.get(20)?,
                    decomposition_policy: r.get(21)?,
                    tier: r.get(22)?,
                    qa_status: r.get(23)?,
                    scenario_id: r.get(24)?,
                    defect_task: r.get(25)?,
                    scenario_amendment: r.get(26)?,
                    // TIER-042: read columns added in migration 021.
                    tier_used: r.get(27)?,
                    escalation_reason: r.get(28)?,
                    // US-CKT-SCHEMA-011/021: read columns added in migration 022.
                    evidence: r.get(29)?,
                    batch_id: r.get(30)?,
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
    pub no_cycle: bool,
    pub assignee: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub parent_task_id: Option<Option<&'a str>>,
    // FIX-DAEMON-r2-tier: tier filter (low|med|high)
    pub tier: Option<&'a str>,
    // FIX-DAEMON-r2-qa: qa_status filter (pass|defect|scenario_error)
    pub qa_status: Option<&'a str>,
    // US-CKT-SCHEMA-006: scenario_id filter
    pub scenario_id: Option<&'a str>,
    // US-CKT-SCHEMA-022: batch_id filter (group by batch invocation)
    pub batch_id: Option<&'a str>,
    /// US-CKT-SCHEMA-044: pagination — max rows to return (None = no limit).
    pub limit: Option<i64>,
    /// US-CKT-SCHEMA-044: pagination — number of rows to skip before returning.
    pub offset: Option<i64>,
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
        // LM-11092: the status filter accepts a comma-separated list and
        // matches any of the given statuses (OR). A single status — the common
        // case, and what every internal caller passes — is the degenerate
        // one-element list. Empty segments (e.g. trailing commas) are ignored.
        let statuses: Vec<&str> = s
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .collect();
        match statuses.as_slice() {
            [] => {}
            [single] => {
                clauses.push("s.status = ?".into());
                vals.push((*single).to_string().into());
            }
            many => {
                let placeholders = vec!["?"; many.len()].join(", ");
                clauses.push(format!("s.status IN ({placeholders})"));
                for st in many {
                    vals.push((*st).to_string().into());
                }
            }
        }
    }
    if let Some(c) = filter.cycle_id {
        clauses.push("s.cycle_id = ?".into());
        vals.push(c.to_string().into());
    } else if filter.no_cycle {
        clauses.push("s.cycle_id IS NULL".into());
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
    if let Some(t) = filter.tier {
        clauses.push("s.tier = ?".into());
        vals.push(t.to_string().into());
    }
    if let Some(q) = filter.qa_status {
        clauses.push("s.qa_status = ?".into());
        vals.push(q.to_string().into());
    }
    // US-CKT-SCHEMA-006: scenario_id filter
    if let Some(sc) = filter.scenario_id {
        clauses.push("s.scenario_id = ?".into());
        vals.push(sc.to_string().into());
    }
    // US-CKT-SCHEMA-022: batch_id filter
    if let Some(bid) = filter.batch_id {
        clauses.push("s.batch_id = ?".into());
        vals.push(bid.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY s.unit_id, s.idx");

    // US-CKT-SCHEMA-044: pagination. Offset without limit isn't useful in
    // SQLite, so when only offset is supplied we apply a sentinel large limit.
    if let Some(lim) = filter.limit {
        sql.push_str(" LIMIT ?");
        vals.push(lim.into());
        if let Some(off) = filter.offset {
            sql.push_str(" OFFSET ?");
            vals.push(off.into());
        }
    } else if let Some(off) = filter.offset {
        sql.push_str(" LIMIT -1 OFFSET ?");
        vals.push(off.into());
    }

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

/// US-CKT-SCHEMA-029: aggregate qa_status histogram for a batch.
///
/// Returns counts of `pass`, `defect`, `scenario_error` and a total row count
/// for tasks matching the given batch_id. Tasks with `qa_status` outside the
/// {pass, defect, scenario_error} set are tallied under `total` only.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct BatchStats {
    pub batch_id: String,
    pub total: u64,
    pub pass: u64,
    pub defect: u64,
    pub scenario_error: u64,
}

pub fn stats_by_batch(conn: &Connection, batch_id: &str) -> Result<BatchStats> {
    // Validation of batch_id format is the route layer's responsibility — repo
    // simply parameterizes the lookup. The partial index idx_tasks_batch_id
    // (migration 022) covers the WHERE clause for O(log n) execution.
    let mut stmt = conn
        .prepare("SELECT qa_status, COUNT(*) FROM tasks WHERE batch_id = ?1 GROUP BY qa_status")?;
    let rows = stmt.query_map([batch_id], |r| {
        let qs: Option<String> = r.get(0)?;
        let n: i64 = r.get(1)?;
        Ok((qs, n))
    })?;
    let mut s = BatchStats {
        batch_id: batch_id.to_string(),
        ..Default::default()
    };
    for row in rows {
        let (qs, n) = row?;
        let n_u: u64 = n.try_into().unwrap_or(0);
        s.total += n_u;
        match qs.as_deref() {
            Some("pass") => s.pass = n_u,
            Some("defect") => s.defect = n_u,
            Some("scenario_error") => s.scenario_error = n_u,
            _ => {}
        }
    }
    Ok(s)
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
    // FIX-DAEMON-001
    pub tier: Option<Option<String>>,
    // TIER-042: tier_used (executed tier — agent reports after run)
    pub tier_used: Option<Option<String>>,
    // TIER-043: escalation_reason — required when tier_used differs from tier
    pub escalation_reason: Option<Option<String>>,
    // FIX-DAEMON-006: QA workflow
    pub qa_status: Option<Option<String>>,
    pub scenario_id: Option<Option<String>>,
    pub defect_task: Option<Option<String>>,
    pub scenario_amendment: Option<Option<String>>,
    // US-CKT-SCHEMA-011: evidence (file:line or reasoning summary)
    pub evidence: Option<Option<String>>,
    // US-CKT-SCHEMA-021: batch_id (ULID of sub-agent batch invocation)
    pub batch_id: Option<Option<String>>,
    // FIX-DAEMON-105: state-machine
    pub blocked_reason: Option<Option<String>>,
    // FIX-DAEMON-107: actor for audit trail ("claude" | "cli" | "external-api" | "system")
    pub actor: Option<String>,
}

pub type CascadeEvent = (&'static str, String);

pub fn update(
    conn: &mut Connection,
    id: &str,
    f: UpdateFields,
) -> Result<(Option<Task>, Vec<CascadeEvent>)> {
    let canonical =
        resolve_id(conn, id)?.ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;
    let old = get(conn, &canonical)?;

    if let Some(ref new_title) = f.title {
        if new_title.trim().is_empty() {
            bail!("INVALID_TITLE: title cannot be empty");
        }
    }

    // FIX-DAEMON-r2-tier: validate tier on update before any other work.
    if let Some(Some(ref new_tier)) = f.tier {
        if !new_tier.is_empty() && !matches!(new_tier.as_str(), "low" | "med" | "high") {
            bail!(
                "INVALID_TIER: tier='{}' is not allowed. Valid: low, med, high",
                new_tier
            );
        }
    }
    // FIX-DAEMON-r2-qa: validate qa_status on update.
    if let Some(Some(ref new_qa)) = f.qa_status {
        if !new_qa.is_empty() && !matches!(new_qa.as_str(), "pass" | "defect" | "scenario_error") {
            bail!(
                "INVALID_QA_STATUS: qa_status='{}' is not allowed. Valid: pass, defect, scenario_error",
                new_qa
            );
        }
    }

    if let Some(Some(new_parent_raw)) = &f.parent_task_id {
        if !new_parent_raw.is_empty() {
            let new_parent_canonical = resolve_id(conn, new_parent_raw)?
                .ok_or_else(|| anyhow::anyhow!("parent_task_id not found: {}", new_parent_raw))?;
            if would_create_cycle(conn, &canonical, &new_parent_canonical)? {
                bail!(
                    "parent_task_id update would create a cycle: {} → {}",
                    canonical,
                    new_parent_canonical
                );
            }
        }
    }

    // FIX-DAEMON-105: state-machine guards
    if let Some(new_status) = &f.status {
        if !matches!(
            new_status.as_str(),
            "todo" | "in_progress" | "done" | "blocked" | "cancelled"
        ) {
            bail!(
                "INVALID_TRANSITION: invalid task status \"{}\". Valid: todo, in_progress, done, blocked, cancelled",
                new_status
            );
        }
        // Enforce state-machine transitions
        if let Some(ref old_task) = old {
            let from = old_task.status.as_str();
            let to = new_status.as_str();
            let valid = matches!(
                (from, to),
                ("todo",        "in_progress")
                | ("todo",      "done")         // direct completion
                | ("todo",      "cancelled")
                | ("todo",      "blocked")
                | ("in_progress", "done")
                | ("in_progress", "blocked")
                | ("in_progress", "cancelled")
                | ("in_progress", "todo")   // re-queue
                | ("blocked",   "todo")
                | ("blocked",   "in_progress")
                | ("blocked",   "done")         // complete while blocked
                | ("blocked",   "cancelled")
                | ("done",      "todo")     // re-open
                | ("done",      "cancelled")
                | ("cancelled", "todo") // re-open
            );
            if !valid && from != to {
                bail!(
                    "INVALID_TRANSITION: task cannot transition from '{}' to '{}'",
                    from,
                    to
                );
            }
        }
        // FIX-DAEMON-r2-task-state: blocked requires blocked_reason (HARD 400 in v3).
        // Round-2 evidence: state-machine guard previously soft-warned, allowing
        // tasks to enter blocked status without an audit trail. v3 plan tightens this:
        // BLOCKED_REASON_REQUIRED is a 400 BadRequest.
        if new_status == "blocked" {
            // Treat empty string the same as missing.
            let has_reason = f
                .blocked_reason
                .as_ref()
                .and_then(|r| r.as_ref())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            // Allow re-issuing blocked → blocked without a new reason if the task is
            // already blocked (idempotent). Only fail when transitioning INTO blocked.
            let entering_blocked = old
                .as_ref()
                .map(|o| o.status.as_str() != "blocked")
                .unwrap_or(true);
            if entering_blocked && !has_reason {
                bail!(
                    "BLOCKED_REASON_REQUIRED: transitioning to status=blocked requires a non-empty blocked_reason"
                );
            }
        }
        // US-CKT-SCHEMA-017 / PDD X8: done requires evidence (HARD 400 in v3).
        // The effective evidence after this update is either the new value
        // explicitly set in `f.evidence` (Some(Some(s))) or, if not present in
        // the patch, the existing value on the task (old_task.evidence).
        // A patch that explicitly clears evidence to null (Some(None)) while
        // transitioning into done must also fail.
        if new_status == "done" {
            let entering_done = old
                .as_ref()
                .map(|o| o.status.as_str() != "done")
                .unwrap_or(true);
            if entering_done {
                // Resolve effective evidence value after this patch.
                let effective_evidence: Option<&str> = match &f.evidence {
                    // Explicitly set in this patch (may be Some(s) or None=clear).
                    Some(inner) => inner.as_deref(),
                    // Not in patch: fall back to existing value.
                    None => old.as_ref().and_then(|o| o.evidence.as_deref()),
                };
                let has_evidence = effective_evidence
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if !has_evidence {
                    bail!(
                        "EVIDENCE_REQUIRED: transitioning to status=done requires a non-empty evidence (file:line or reasoning summary)"
                    );
                }
            }
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
    push_str_opt(&mut sets, &mut vals, "tier = ?", &f.tier);
    // TIER-042: tier_used + escalation_reason
    push_str_opt(&mut sets, &mut vals, "tier_used = ?", &f.tier_used);
    push_str_opt(
        &mut sets,
        &mut vals,
        "escalation_reason = ?",
        &f.escalation_reason,
    );
    push_str_opt(&mut sets, &mut vals, "qa_status = ?", &f.qa_status);
    push_str_opt(&mut sets, &mut vals, "scenario_id = ?", &f.scenario_id);
    push_str_opt(&mut sets, &mut vals, "defect_task = ?", &f.defect_task);
    push_str_opt(
        &mut sets,
        &mut vals,
        "scenario_amendment = ?",
        &f.scenario_amendment,
    );
    // US-CKT-SCHEMA-011/021: evidence + batch_id
    push_str_opt(&mut sets, &mut vals, "evidence = ?", &f.evidence);
    push_str_opt(&mut sets, &mut vals, "batch_id = ?", &f.batch_id);
    // FIX-DAEMON-105: blocked_reason (stored in body for now — no dedicated column yet)
    // When blocked, append reason to body if provided
    if let Some(Some(reason)) = &f.blocked_reason {
        if !reason.is_empty() {
            // Append blocked_reason to body as a structured note
            sets.push("body = body || ?");
            vals.push(format!("\n\n[BLOCKED_REASON]: {}", reason).into());
        }
    }

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
        return Ok((get(conn, &canonical)?, Vec::new()));
    }

    // FIX-DAEMON-005: Wrap status update + runs + cascade + activity_log in a single transaction.
    let tx = conn.transaction()?;

    {
        vals.push(canonical.clone().into());
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", sets.join(", "));
        let params_iter = rusqlite::params_from_iter(vals.iter());
        tx.execute(&sql, params_iter)?;
    }

    if let Some(status) = &f.status {
        if status == "in_progress" {
            let has_open_run: bool = {
                tx.query_row(
                    "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND ended_at IS NULL",
                    params![canonical],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    > 0
            };
            if !has_open_run {
                let agent = f
                    .assignee
                    .as_ref()
                    .and_then(|a| a.clone())
                    .unwrap_or_else(|| "main".into());
                let run_id = new_id("RUN");
                let ts = now_ms();
                tx.execute(
                    "INSERT INTO runs (id, task_id, session_id, agent, started_at, status)
                     VALUES (?1, ?2, NULL, ?3, ?4, 'started')",
                    params![run_id, canonical, agent, ts],
                )?;
            }
        }
        if status == "done" || status == "cancelled" || status == "blocked" {
            let (result, run_status) = match status.as_str() {
                "done" => ("success", "finished"),
                "blocked" => ("blocked", "aborted"),
                _ => ("aborted", "aborted"),
            };
            let ts = now_ms();
            tx.execute(
                "UPDATE runs SET ended_at = ?1, result = ?2, status = ?3
                 WHERE task_id = ?4 AND ended_at IS NULL",
                params![ts, result, run_status, canonical],
            )?;
        }
        if status == "in_progress" {
            // Check plan active guard
            let (plan_status, plan_title, plan_id): (String, String, String) = tx
                .query_row(
                    "SELECT pl.status, pl.title, pl.id FROM plans pl
                 JOIN units u ON u.plan_id = pl.id
                 JOIN tasks t ON t.unit_id = u.id
                 WHERE t.id = ?1",
                    params![canonical],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or_else(|_| ("active".into(), "".into(), "".into()));
            if plan_status != "active" {
                bail!(
                    "Cannot start task: plan \"{}\" is {}. Approve it first: clawket plan approve {}",
                    plan_title, plan_status, plan_id
                );
            }
            // Check cycle active guard
            let cycle_check: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT c.id, c.status, c.title FROM cycles c
                 JOIN tasks t ON t.cycle_id = c.id
                 WHERE t.id = ?1",
                    params![canonical],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match cycle_check {
                None => bail!(
                    "MISSING_CYCLE_ID: Cannot start task: no cycle assigned. Assign a cycle first: clawket task update {} --cycle <CYC-ID>",
                    canonical
                ),
                Some((_, cyc_status, cyc_title)) if cyc_status != "active" => bail!(
                    "INVALID_TRANSITION: Cannot start task: cycle \"{}\" is {}. Activate it first.",
                    cyc_title, cyc_status
                ),
                _ => {}
            }
        }
    }

    // FIX-DAEMON-107: Record audit log entries inside transaction for all field mutations.
    // Also keep writing to activity_log (legacy) for the rollup job compatibility.
    if let Some(ref old_task) = old {
        // Determine actor: explicit > assignee field change > current assignee > system
        let actor = f
            .actor
            .as_deref()
            .or_else(|| f.assignee.as_ref().and_then(|a| a.as_deref()))
            .or(old_task.assignee.as_deref())
            .unwrap_or("system");
        let actor = match actor {
            "claude" | "cli" | "external-api" | "system" => actor,
            _ => "cli",
        };

        // Helper macro-like closure to write one audit row (inside tx)
        let write_audit = |field: &str, op: &str, old: Option<&str>, new: Option<&str>| {
            let audit_id = new_id("AUD");
            let ts = now_ms();
            let at = crate::models::ms_to_iso(ts);
            let _ = tx.execute(
                "INSERT INTO audit_log (id, entity_type, entity_id, op_type, field, old_value, new_value, actor, at)
                 VALUES (?1, 'task', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![audit_id, canonical, op, field, old, new, actor, at],
            );
        };

        // status change — also mirrors to activity_log
        if let Some(new_status) = &f.status {
            if new_status != &old_task.status {
                let log_id = new_id("LOG");
                let ts = now_ms();
                let _ = tx.execute(
                    "INSERT INTO activity_log (id, entity_type, entity_id, action, field, old_value, new_value, actor, created_at)
                     VALUES (?1, 'task', ?2, 'status_change', 'status', ?3, ?4, ?5, ?6)",
                    params![log_id, canonical, old_task.status, new_status, actor, ts],
                );
                write_audit(
                    "status",
                    "status_change",
                    Some(&old_task.status),
                    Some(new_status),
                );
            }
        }
        // title change
        if let Some(ref new_title) = f.title {
            if new_title != &old_task.title {
                write_audit("title", "updated", Some(&old_task.title), Some(new_title));
            }
        }
        // body change (Option<Option<String>>: Some(Some(v)) = set, Some(None) = clear)
        if let Some(ref new_body_opt) = f.body {
            let old_body_str: &str = &old_task.body;
            let new_body_str: Option<&str> = new_body_opt.as_deref();
            let new_body_for_cmp = new_body_str.unwrap_or("");
            if old_body_str != new_body_for_cmp {
                write_audit("body", "updated", Some(old_body_str), new_body_str);
            }
        }
        // priority change (priority is String, not Option<String>)
        if let Some(ref new_priority) = f.priority {
            let old_p = old_task.priority.as_str();
            if new_priority != old_p {
                write_audit("priority", "updated", Some(old_p), Some(new_priority));
            }
        }
        // assignee change
        if let Some(ref new_assignee_opt) = f.assignee {
            let old_a: Option<&str> = old_task.assignee.as_deref();
            let new_a: Option<&str> = new_assignee_opt.as_deref();
            if old_a != new_a {
                write_audit("assignee", "updated", old_a, new_a);
            }
        }
        // FIX-DAEMON-r2-tier: tier change audit
        if let Some(ref new_tier_opt) = f.tier {
            let old_t: Option<&str> = old_task.tier.as_deref();
            let new_t: Option<&str> = new_tier_opt.as_deref();
            if old_t != new_t {
                write_audit("tier", "updated", old_t, new_t);
            }
        }
        // FIX-DAEMON-r2-qa: qa_status change audit
        if let Some(ref new_qa_opt) = f.qa_status {
            let old_q: Option<&str> = old_task.qa_status.as_deref();
            let new_q: Option<&str> = new_qa_opt.as_deref();
            if old_q != new_q {
                write_audit("qa_status", "updated", old_q, new_q);
            }
        }
        // FIX-DAEMON-r2-qa: scenario_id audit
        if let Some(ref new_sc_opt) = f.scenario_id {
            let old_s: Option<&str> = old_task.scenario_id.as_deref();
            let new_s: Option<&str> = new_sc_opt.as_deref();
            if old_s != new_s {
                write_audit("scenario_id", "updated", old_s, new_s);
            }
        }
        // FIX-DAEMON-r2-qa: defect_task audit
        if let Some(ref new_dt_opt) = f.defect_task {
            let old_d: Option<&str> = old_task.defect_task.as_deref();
            let new_d: Option<&str> = new_dt_opt.as_deref();
            if old_d != new_d {
                write_audit("defect_task", "updated", old_d, new_d);
            }
        }
        // FIX-DAEMON-r2-qa: scenario_amendment audit
        if let Some(ref new_sa_opt) = f.scenario_amendment {
            let old_sa: Option<&str> = old_task.scenario_amendment.as_deref();
            let new_sa: Option<&str> = new_sa_opt.as_deref();
            if old_sa != new_sa {
                write_audit("scenario_amendment", "updated", old_sa, new_sa);
            }
        }
        // US-CKT-SCHEMA-011: evidence audit
        if let Some(ref new_ev_opt) = f.evidence {
            let old_ev: Option<&str> = old_task.evidence.as_deref();
            let new_ev: Option<&str> = new_ev_opt.as_deref();
            if old_ev != new_ev {
                write_audit("evidence", "updated", old_ev, new_ev);
            }
        }
        // US-CKT-SCHEMA-021: batch_id audit
        if let Some(ref new_bid_opt) = f.batch_id {
            let old_bid: Option<&str> = old_task.batch_id.as_deref();
            let new_bid: Option<&str> = new_bid_opt.as_deref();
            if old_bid != new_bid {
                write_audit("batch_id", "updated", old_bid, new_bid);
            }
        }
    }

    // FIX-DAEMON-005: cascade also inside transaction (complex — we call a helper that
    // operates on the connection, but transaction is committed first to avoid deadlock
    // with recursive selects). We commit here then call cascade.
    tx.commit()?;

    // Post-commit: cascade completion (requires conn, not tx)
    // FIX-DAEMON-106: cascade returns (event_name, entity_id) pairs for SSE emit at route layer
    // LM-11093: dispatch on either axis that can finish a container.
    //
    //   status → CONTAINER_TERMINAL (not the narrower done|cancelled): blocking
    //            the last open task finishes the container just as surely as
    //            completing it.
    //   cycle_id → DETACHING (`Some(None)`) the last non-terminal task removes it
    //            from the plan's scheduled set, which finishes the plan. A detach
    //            carries NO status field, so a status-only dispatch never fires
    //            for it.
    //
    // Both axes needed widening for the same reason: leaving either narrow makes
    // the outcome depend on transition ORDER. Same end state, two answers —
    // finish-then-defer left the plan open forever while defer-then-finish
    // closed it, because only the second ends on a dispatching transition.
    //
    // ATTACHING (`Some(Some(id))`) is deliberately excluded. It adds work to the
    // schedule, so it can never be what finishes a container — and dispatching on
    // it actively breaks recovery: re-attaching a still-blocked task to a fresh
    // cycle would re-close the plan the user just re-opened, since the scheduled
    // set is container-terminal again the moment the task lands in it.
    //
    //   qa_status → CLEARING a `defect` verdict settles the task too, because
    //            `container_terminal` reads that field as well. `routes::tasks`
    //            accepts `qa_status` independently of `status`, so a lone
    //            `{"qa_status":"pass"}` patch on the last blocked defect row is a
    //            settling transition that a status-only dispatch would miss —
    //            leaving the container open until some unrelated edit fires. Same
    //            order-dependence the two axes above were widened to remove.
    let detaching = matches!(f.cycle_id, Some(None));
    let clearing_defect = matches!(&f.qa_status, Some(v) if v.as_deref() != Some("defect"))
        && old
            .as_ref()
            .is_some_and(|t| t.qa_status.as_deref() == Some("defect"));
    let dispatch = f
        .status
        .as_deref()
        .is_some_and(|s| CONTAINER_TERMINAL.contains(&s))
        || detaching
        || clearing_defect;
    // On a detach the row no longer names the cycle it left, so read it from the
    // pre-update snapshot and hand it to the cascade. Without this the cycle arm
    // has nothing to resolve and the plan completes over an `active` cycle.
    let left_cycle = if detaching {
        old.as_ref().and_then(|t| t.cycle_id.clone())
    } else {
        None
    };
    let cascade_events: Vec<CascadeEvent> = if dispatch {
        cascade_complete(conn, &canonical, left_cycle.as_deref())?
    } else {
        Vec::new()
    };

    // FIX-DAEMON-011: Auto-extract DECISION: lines from body changes
    if let Some(Some(new_body)) = &f.body {
        let task = get(conn, &canonical)?;
        if let Some(ref t) = task {
            extract_decisions(conn, t, new_body)?;
        }
    }

    Ok((get(conn, &canonical)?, cascade_events))
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
    let canonical =
        resolve_id(conn, id)?.ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;
    conn.execute(
        "INSERT OR IGNORE INTO task_labels (task_id, label) VALUES (?1, ?2)",
        params![canonical, label],
    )?;
    get(conn, &canonical)
}

pub fn remove_label(conn: &Connection, id: &str, label: &str) -> Result<Option<Task>> {
    let canonical =
        resolve_id(conn, id)?.ok_or_else(|| anyhow::anyhow!("Task not found: {}", id))?;
    conn.execute(
        "DELETE FROM task_labels WHERE task_id = ?1 AND label = ?2",
        params![canonical, label],
    )?;
    get(conn, &canonical)
}

/// FIX-DAEMON-106: cascade_complete returns list of (event_name, entity_id) for SSE emit
///
/// LM-11057 (US-CLAWKETD-CASCADE-001): plan/cycle completion is derived directly from
/// the set of tasks that belong to the plan/cycle — not from intermediate per-unit
/// derivations. The previous implementation required `all units have ≥1 task && all
/// tasks terminal`, which a single empty unit could block forever. Units are pure
/// grouping (FIX-DAEMON-004), so they have no completion state to derive — the only
/// meaningful question is "are all of THIS plan's tasks terminal?".
///
/// Definition: a plan (or cycle) auto-completes iff
///   (a) it owns ≥ 1 task, AND
///   (b) every task it owns is container-terminal — `container_terminal(status,
///       qa_status)`, i.e. done | cancelled | blocked, EXCEPT a `blocked` row
///       carrying `qa_status = 'defect'` (an open QA defect is work inside the
///       plan, not an external blocker).
///
/// The criterion reads two fields, not one. Its SQL twin is
/// `repo::plans::count_completion_residue`; `repo::cycles::assert_no_todo_residue`
/// carries the same clause for the manual gate. All three must move together.
///
/// An empty plan / cycle (no tasks) is a cascade no-op — auto-completing a container
/// that has no work declared yet would erase user intent.
///
/// LM-11093 fixes two ways a plan became permanently uncompletable, which are
/// independent and both had to be closed:
///
/// 1. **`blocked` counted as unfinished work.** A blocked task waits on something
///    outside this plan, so holding the plan open cannot resolve it. The manual
///    cycle gate already read it that way (`cycles::assert_no_todo_residue`,
///    PDD-230, which spells the same three statuses directly in SQL); the plan
///    gates did not, so a cycle could reach `completed` while its plan refused,
///    citing a task that cycle had accepted. Both cascade arms and both plan
///    residue gates now agree on `container_terminal` — including its carve-out
///    for a QA defect, which `blocked` also spells.
///
/// 2. **Backlog tasks counted as this plan's remaining work.** Detaching a task
///    (`task update --cycle ""`, `cycle_id IS NULL`) is how a user says "not this
///    round". `tasks.unit_id` is NOT NULL (`migrations/001_initial.sql:106`) so the
///    task keeps its unit, and the plan filter JOINs through units — deferred work
///    stayed in the completion set. Only tasks attached to a cycle count now.
///
/// Deferred work is not lost: the plan can be re-opened (`completed → active`,
/// `repo::plans::update`) and the task stays visible in the backlog view
/// (`task list --no-cycle`) throughout. Re-scheduling it needs an active cycle,
/// which may mean creating one — completed cycles do not restart (v3.0).
///
/// The exclusion is keyed on "not attached to a cycle", which is slightly wider
/// than "the user deferred it": `repo::cycles::delete` also nulls `cycle_id` on
/// its tasks, so deleting a cycle moves live tasks to the backlog and out of the
/// plan's completion set. That is acceptable — deleting a cycle is itself a
/// deliberate act and the tasks remain listed (`task list --no-cycle`). If the
/// two ever need separating, the durable fix is a `deferred_at` column rather
/// than overloading `cycle_id`.
///
/// `left_cycle` carries the cycle the trigger just VACATED, because on the detach
/// path the task can no longer name it. `cycle_id` is set to NULL inside the
/// transaction and committed before this function re-reads the row, so
/// `task.cycle_id` is already `None` and the cycle arm's `Some(cid)` guard would
/// skip — completing the plan while its cycle stayed `active`. That is the exact
/// state the cycle arm's own comment argues must never exist and that
/// `routes::plans` rejects with `PLAN_HAS_ACTIVE_CYCLES` (PDD-231), so the two
/// completion paths would write different databases. Callers pass the pre-update
/// `cycle_id` on a detach and `None` otherwise; the arm falls back to the task's
/// own `cycle_id` for every non-detach transition.
pub fn cascade_complete(
    conn: &mut Connection,
    task_id: &str,
    left_cycle: Option<&str>,
) -> Result<Vec<CascadeEvent>> {
    let mut cascaded: Vec<CascadeEvent> = Vec::new();
    let task = match get(conn, task_id)? {
        Some(t) => t,
        None => return Ok(cascaded),
    };

    // The trigger must have stopped holding its containers open — either by
    // reaching a container-terminal status, or by leaving the schedule entirely
    // (backlog, `cycle_id IS NULL`). A detached task is outside the plan's
    // scheduled set no matter what status it carries, so gating on status alone
    // would drop exactly the detach case the dispatch site now forwards.
    let trigger_settled =
        container_terminal(&task.status, task.qa_status.as_deref()) || task.cycle_id.is_none();
    if !trigger_settled {
        return Ok(cascaded);
    }

    // Plan-task-direct cascade. `ListFilter::plan_id` JOINs through units, so the
    // result set is exactly the tasks owned by this plan — empty units contribute
    // zero rows and therefore cannot block completion.
    if let Some(unit) = units::get(conn, &task.unit_id)? {
        let owned = list(
            conn,
            ListFilter {
                plan_id: Some(&unit.plan_id),
                ..Default::default()
            },
        )?;
        // LM-11093: backlog tasks are deferred work, not this plan's remaining
        // work. Dropped here rather than inside `list()` so the plan's task
        // listing (dashboards, `task list --plan`) keeps showing everything;
        // only the completion arithmetic narrows.
        let scheduled: Vec<Task> = owned.into_iter().filter(|t| t.cycle_id.is_some()).collect();

        // Auto-complete only when every scheduled task is container-terminal AND
        // at least one was actually `done`. A plan whose tasks are *all*
        // cancelled was not completed — it was emptied/corrected (e.g.
        // mis-created tasks cancelled to re-author them). Cascading "completed"
        // there closes the plan and makes the active cycle unrestartable (#51);
        // `cancelled`-only stays open.
        //
        // `any(done)` is also what keeps the two empty-ish cases open, and it is
        // the ONLY term doing so — it implies `!scheduled.is_empty()`, so no
        // separate emptiness guard is needed (one was tried; mutation testing
        // showed it could never decide the outcome):
        //   - plan owns no tasks at all      → no work declared yet
        //   - plan's work is entirely backlog → all of it deferred, none finished
        // Both are right to stay open, for different reasons. Relaxing
        // `any(done)` would silently close both.
        let plan_done = scheduled
            .iter()
            .all(|t| container_terminal(&t.status, t.qa_status.as_deref()))
            && scheduled.iter().any(|t| t.status == "done");
        if plan_done {
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
                    cascaded.push(("plan:updated", plan.id.clone()));
                }
            }
        }
    }

    // Cycle-task-direct cascade. All tasks attached to this cycle must be
    // container-terminal, and the cycle must own at least one task.
    //
    // LM-11093: this arm reads the SAME `CONTAINER_TERMINAL` as the plan arm,
    // and it must. Narrowing it here — so a blocked task would leave its cycle
    // live, on the reasoning that a completed cycle cannot be restarted and
    // auto-closing one around a blocker strands it — was tried and reverted.
    // That reasoning does not survive contact with the rest of the system:
    //
    //   - Starting a task requires `plan.status == "active"` as well as an
    //     active cycle (the guards above), so leaving the cycle open buys
    //     nothing — the plan must be re-opened either way.
    //   - "A completed plan has no active cycle" is a system invariant, asserted
    //     twice independently: `routes::plans` re-counts active cycles after its
    //     cascade close and rejects with `PLAN_HAS_ACTIVE_CYCLES` (PDD-231), and
    //     `routes::discover` closes prior-round cycles so rounds never coexist
    //     (DOGFOOD-004). Letting this arm lag the plan arm would make the two
    //     completion paths produce different database states — the route path
    //     upholding the invariant, the cascade path quietly breaking it.
    //
    // `cycles::assert_no_todo_residue` (PDD-230) already accepts `blocked` for
    // the manual `cycle complete`, so this arm now agrees with the gate the user
    // reaches by hand.
    //
    // Recovery for a blocked task after the containers close: re-open the plan
    // (`completed → active`), create and activate a cycle, re-attach. The old
    // cycle cannot restart, which is v3.0 behaviour and not this change.
    //
    // On a detach the task no longer names the cycle it just left — `cycle_id` is
    // already NULL by the time this function re-reads the row — so `left_cycle`
    // supplies it. Without that, the arm below would be unreachable on exactly
    // the axis this cascade was widened to cover, and the plan would complete
    // over an `active` cycle. Leaving that cycle open also breaks the recovery
    // described above: `UNIT_HAS_ACTIVE_CYCLE` (PDD A8) refuses to activate a new
    // cycle on a unit that still has one.
    //
    // No backlog filter inside the arm: the cycle is resolved by id, and a
    // detached task is no longer in it.
    if let Some(cid) = left_cycle.or(task.cycle_id.as_deref()) {
        let cycle_tasks = list(
            conn,
            ListFilter {
                cycle_id: Some(cid),
                ..Default::default()
            },
        )?;
        // Same guard as the plan cascade (#51): a cycle whose tasks are all
        // cancelled was emptied, not completed. Require at least one `done` so
        // cancelling every task to correct it does not auto-complete (and thus
        // freeze) the cycle.
        let cycle_done = !cycle_tasks.is_empty()
            && cycle_tasks
                .iter()
                .all(|t| container_terminal(&t.status, t.qa_status.as_deref()))
            && cycle_tasks.iter().any(|t| t.status == "done");
        if cycle_done {
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
                    cascaded.push(("cycle:updated", cycle.id.clone()));
                }
            }
        }
    }

    Ok(cascaded)
}

// Auto-extract `DECISION:` prefix lines from task body and persist them as
// `type=decision` knowledge entries. Hash-dedupe prevents duplicates on
// repeated body updates.
fn extract_decisions(conn: &Connection, task: &Task, body: &str) -> Result<()> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.trim_start().starts_with("DECISION:"))
        .collect();

    if lines.is_empty() {
        return Ok(());
    }

    for line in lines {
        let decision_text = line.trim_start().trim_start_matches("DECISION:").trim();
        if decision_text.is_empty() {
            continue;
        }

        // Hash the decision text for deduplication
        let mut hasher = DefaultHasher::new();
        decision_text.hash(&mut hasher);
        let hash = hasher.finish();
        let hash_tag = format!("decision-hash:{:016x}", hash);

        // Check if a knowledge entry with this hash already exists for this task.
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge WHERE task_id = ?1 AND type = 'decision'
             AND content LIKE ?2",
                params![task.id, format!("%{}%", hash_tag)],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if exists {
            continue;
        }

        let content = format!("{}\n\n<!-- {} -->", decision_text, hash_tag);
        let _ = knowledge::create(
            conn,
            knowledge::CreateInput {
                task_id: Some(&task.id),
                unit_id: None,
                plan_id: None,
                type_: "decision",
                title: &format!(
                    "Decision: {}",
                    &decision_text[..decision_text.len().min(80)]
                ),
                content: Some(&content),
                content_format: Some("md"),
                parent_id: None,
            },
        );
    }

    Ok(())
}

fn parent_of(conn: &Connection, id: &str) -> Result<Option<String>> {
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT parent_task_id FROM tasks WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row.flatten())
}

/// Walks `start`'s parent chain looking for either a cycle in the existing
/// data or a back-edge to `descendant`. Returns true if the chain is unsafe.
const TREE_WALK_MAX_DEPTH: usize = 1024;

fn would_create_cycle(conn: &Connection, descendant: &str, new_parent: &str) -> Result<bool> {
    if descendant == new_parent {
        return Ok(true);
    }
    let mut current = new_parent.to_string();
    let mut seen = std::collections::HashSet::new();
    seen.insert(current.clone());
    for _ in 0..TREE_WALK_MAX_DEPTH {
        match parent_of(conn, &current)? {
            None => return Ok(false),
            Some(p) => {
                if p == descendant {
                    return Ok(true);
                }
                if !seen.insert(p.clone()) {
                    // Existing cycle in DB — refuse to entrench it further.
                    return Ok(true);
                }
                current = p;
            }
        }
    }
    // Depth cap exceeded — treat as cycle for safety.
    Ok(true)
}

fn has_existing_cycle(conn: &Connection, start: &str) -> Result<bool> {
    let mut current = start.to_string();
    let mut seen = std::collections::HashSet::new();
    seen.insert(current.clone());
    for _ in 0..TREE_WALK_MAX_DEPTH {
        match parent_of(conn, &current)? {
            None => return Ok(false),
            Some(p) => {
                if !seen.insert(p.clone()) {
                    return Ok(true);
                }
                current = p;
            }
        }
    }
    Ok(true)
}

/// Walk the parent chain of `task_id` from immediate parent up to root,
/// terminating early on cycle or depth cap. Returns the chain in
/// **parent-first** order (closest parent first, root last). The seed task
/// itself is **not** included in the result.
pub fn ancestors(conn: &Connection, task_id: &str, max_depth: usize) -> Result<Vec<Task>> {
    let canonical = match resolve_id(conn, task_id)? {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(canonical.clone());
    let mut current = canonical;
    let cap = max_depth.min(TREE_WALK_MAX_DEPTH);
    for _ in 0..cap {
        let parent_id = match parent_of(conn, &current)? {
            Some(p) => p,
            None => break,
        };
        if !seen.insert(parent_id.clone()) {
            break;
        }
        let parent = match get(conn, &parent_id)? {
            Some(p) => p,
            None => break,
        };
        out.push(parent);
        current = parent_id;
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct DescendantNode {
    pub task: Task,
    pub depth: i64,
}

/// Walk the children tree rooted at `task_id`. Children of the same parent
/// are returned in `idx` order. The seed itself is **not** included.
/// `max_depth` is 1-based (depth=1 → immediate children only). Order is
/// `dfs` (pre-order) or `bfs` (level-order).
pub fn descendants(
    conn: &Connection,
    task_id: &str,
    max_depth: usize,
    bfs: bool,
    node_cap: usize,
) -> Result<Vec<DescendantNode>> {
    let canonical = match resolve_id(conn, task_id)? {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let depth_cap = max_depth.clamp(1, TREE_WALK_MAX_DEPTH);
    let cap = node_cap.max(1);
    let mut out: Vec<DescendantNode> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(canonical.clone());

    if bfs {
        let mut queue: std::collections::VecDeque<(String, i64)> =
            std::collections::VecDeque::new();
        queue.push_back((canonical, 0));
        while let Some((parent_id, parent_depth)) = queue.pop_front() {
            if parent_depth as usize >= depth_cap {
                continue;
            }
            for child in children_of(conn, &parent_id)? {
                if !seen.insert(child.id.clone()) {
                    continue;
                }
                let depth = parent_depth + 1;
                queue.push_back((child.id.clone(), depth));
                out.push(DescendantNode { task: child, depth });
                if out.len() >= cap {
                    return Ok(out);
                }
            }
        }
    } else {
        let mut stack: Vec<(String, i64)> = Vec::new();
        let initial_children = children_of(conn, &canonical)?;
        // Push in reverse so DFS pre-order matches idx ascending.
        for child in initial_children.into_iter().rev() {
            stack.push((child.id, 1));
        }
        while let Some((id, depth)) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let task = match get(conn, &id)? {
                Some(t) => t,
                None => continue,
            };
            out.push(DescendantNode { task, depth });
            if out.len() >= cap {
                return Ok(out);
            }
            if (depth as usize) < depth_cap {
                for child in children_of(conn, &id)?.into_iter().rev() {
                    stack.push((child.id, depth + 1));
                }
            }
        }
    }
    Ok(out)
}

fn children_of(conn: &Connection, parent_id: &str) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE parent_task_id = ?1 ORDER BY idx")?;
    let rows = stmt.query_map(params![parent_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(t) = get(conn, &r?)? {
            out.push(t);
        }
    }
    Ok(out)
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
    let mut stmt =
        conn.prepare("SELECT depends_on_task_id FROM task_depends_on WHERE task_id = ?1")?;
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

// Node v2.2.1 parity: if the query contains FTS5 operators the user likely
// knows what they're typing, pass through verbatim; otherwise split on
// whitespace and append a prefix wildcard to each term so that "rust clap"
// matches "rustfmt claps" style prefixes.
fn build_fts_query(trimmed: &str) -> String {
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
    if terms.is_empty() {
        String::new()
    } else {
        terms.join(" ")
    }
}

pub fn keyword_search(conn: &Connection, query: &str, limit: i64) -> Result<Vec<Task>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = build_fts_query(trimmed);
    let mut stmt = match conn.prepare(
        "SELECT t.id FROM tasks_fts f
         JOIN tasks t ON t.rowid = f.rowid
         WHERE tasks_fts MATCH ?1
         ORDER BY bm25(tasks_fts) LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let ids: Vec<String> = stmt
        .query_map(params![fts_query, limit], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    let mut out = Vec::new();
    for id in ids {
        if let Some(t) = get(conn, &id)? {
            out.push(t);
        }
    }
    Ok(out)
}

/// (RL-U3-05 / LM-140) Search over envelope `intent` / `prompt_template` /
/// `success_criteria` only — bypasses `tasks.title` + `tasks.body`. Active
/// envelopes only (superseded ones don't surface). De-dups when a task has
/// multiple matching envelope versions, keeping the best-ranked hit.
pub fn keyword_search_envelope_only(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<Task>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let fts_query = build_fts_query(trimmed);
    // Two-pass: rank inside the FTS, then fold to active task_ids in Rust.
    // SQLite rejects `MIN(bm25(...))` with `GROUP BY e.task_id` because bm25
    // isn't a standard aggregate; an inner ranked subquery sidesteps that.
    let mut stmt = match conn.prepare(
        "SELECT e.task_id
         FROM (
             SELECT rowid AS frow, bm25(task_envelopes_fts) AS rank
             FROM task_envelopes_fts
             WHERE task_envelopes_fts MATCH ?1
             ORDER BY rank
         ) ranked
         JOIN task_envelopes e ON e.rowid = ranked.frow
         WHERE e.superseded_by IS NULL
         LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let ids: Vec<String> = stmt
        .query_map(params![fts_query, limit], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    let mut out = Vec::new();
    for id in ids {
        if let Some(t) = get(conn, &id)? {
            out.push(t);
        }
    }
    Ok(out)
}

pub fn store_embedding(conn: &Connection, task_id: &str, embedding: &[f32]) -> Result<()> {
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            embedding.as_ptr() as *const u8,
            std::mem::size_of_val(embedding),
        )
    };
    let _ = conn.execute("DELETE FROM vec_tasks WHERE task_id = ?1", params![task_id]);
    let _ = conn.execute(
        "INSERT INTO vec_tasks (task_id, embedding) VALUES (?1, ?2)",
        params![task_id, bytes],
    );
    Ok(())
}

pub fn vector_search(conn: &Connection, embedding: &[f32], limit: i64) -> Result<Vec<(Task, f32)>> {
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            embedding.as_ptr() as *const u8,
            std::mem::size_of_val(embedding),
        )
    };
    let mut stmt = match conn.prepare(
        "SELECT task_id, distance FROM vec_tasks
         WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map(params![bytes, limit], |r| {
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
        if let Some(t) = get(conn, &id)? {
            out.push((t, distance));
        }
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
                auto_advance: false,
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
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();
        let cycle = cycles::create(
            &db.conn,
            cycles::CreateInput {
                project_id: &project.id,
                unit_id: &unit.id,
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
    fn allows_task_creation_under_draft_plan_but_blocks_start() {
        let mut s = setup(false);
        let task = create(
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
        let f = UpdateFields {
            status: Some("in_progress".to_string()),
            ..UpdateFields::default()
        };
        let err = update(&mut s.db.conn, &task.id, f).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("plan") && msg.contains("draft"),
            "expected start-time draft guard, got: {msg}"
        );
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
        assert_eq!(t2.ticket_number.as_deref(), Some("TB-2"));
        assert_eq!(t2.depends_on, vec![t.id.clone()]);

        let by_ticket = get(&s.db.conn, "TB-1").unwrap().unwrap();
        assert_eq!(by_ticket.id, t.id);
    }

    #[test]
    fn create_rejects_empty_title() {
        let mut s = setup(true);
        for empty in ["", "   ", "\t\n"] {
            let err = create(
                &mut s.db.conn,
                CreateInput {
                    unit_id: &s.unit_id,
                    title: empty,
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
            .unwrap_err();
            assert!(
                err.to_string().starts_with("INVALID_TITLE:"),
                "expected INVALID_TITLE prefix, got: {err}"
            );
        }
    }

    #[test]
    fn update_rejects_empty_title() {
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

        for empty in ["", "   ", "\t\n"] {
            let f = UpdateFields {
                title: Some(empty.to_string()),
                ..UpdateFields::default()
            };
            let err = update(&mut s.db.conn, &t.id, f).unwrap_err();
            assert!(
                err.to_string().starts_with("INVALID_TITLE:"),
                "expected INVALID_TITLE prefix, got: {err}"
            );
        }

        // Sanity check: non-empty title still works.
        let f = UpdateFields {
            title: Some("T1-renamed".to_string()),
            ..UpdateFields::default()
        };
        let (updated, _events) = update(&mut s.db.conn, &t.id, f).unwrap();
        assert_eq!(updated.unwrap().title, "T1-renamed");
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
        .0
        .unwrap();
        assert_eq!(started.status, "in_progress");
        assert!(started.started_at.is_some());

        // FIX-DAEMON-004: Units are pure grouping entities (no status). Task started
        // does not flip unit state — only the task carries lifecycle.
        let _unit = units::get(&s.db.conn, &s.unit_id).unwrap().unwrap();
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

        update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:cascade".into())),
                ..Default::default()
            },
        )
        .unwrap();

        // FIX-DAEMON-004: Units have no status; only plan/cycle cascade.
        let _unit = units::get(&s.db.conn, &s.unit_id).unwrap().unwrap();
        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(plan.status, "completed");
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(cycle.status, "completed");
    }

    // LM-11057 (US-CLAWKETD-CASCADE-001): a plan with multiple units where ONE
    // unit owns zero tasks must still cascade-complete once every existing
    // task is terminal. Pre-fix, the empty unit kept `all_plan_done`
    // permanently false because plan completion was derived from per-unit
    // all-terminal checks gated on `!unit_tasks.is_empty()`. v20 plan
    // (PLAN-01KS53TRCHW71NEKB6V6KV23DW) reproduced exactly this case in
    // production.
    #[test]
    fn cascade_completes_plan_when_a_unit_is_empty() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        // s.unit_id is the "filled" unit; add a sibling unit that owns zero tasks.
        let empty_unit = units::create(
            &s.db.conn,
            units::CreateInput {
                plan_id: &s.plan_id,
                title: "U-empty",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap()
        .unwrap();

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

        update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:empty-unit-cascade".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "completed",
            "plan must complete via cascade even when a sibling unit owns no tasks; empty_unit={}",
            empty_unit.id
        );
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(cycle.status, "completed");
    }

    // LM-11057 (US-CLAWKETD-CASCADE-001): cascade must treat `cancelled` as a
    // terminal status alongside `done`, and must NOT fire until EVERY task in
    // the plan reaches a terminal state. v20 plan in the field carried
    // 16 done + 10 cancelled — the cascade definition has to admit that mix.
    #[test]
    fn cascade_completes_plan_with_mixed_done_and_cancelled() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let mk = |conn: &mut Connection, unit_id: &str, cycle_id: &str, title: &str| -> Task {
            create(
                conn,
                CreateInput {
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
            .unwrap()
        };

        let t1 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "done-1");
        let t2 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "cancel-1");
        let t3 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "done-2");

        update(
            &mut s.db.conn,
            &t1.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:done-1".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &t2.id,
            UpdateFields {
                status: Some("cancelled".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let plan_mid = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan_mid.status, "active",
            "plan must remain active while a non-terminal task survives"
        );

        update(
            &mut s.db.conn,
            &t3.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:done-2".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(plan.status, "completed");
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(cycle.status, "completed");
    }

    // #51: cancelling EVERY task in a cycle must NOT auto-complete the cycle or
    // plan. All-cancelled means the work was emptied/corrected, not finished;
    // auto-completing freezes the cycle (completed cycles cannot restart) and
    // closes the plan, breaking the mis-create → cancel → re-author flow.
    #[test]
    fn cascade_does_not_complete_when_all_tasks_cancelled() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let mk = |conn: &mut Connection, unit_id: &str, cycle_id: &str, title: &str| -> Task {
            create(
                conn,
                CreateInput {
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
            .unwrap()
        };

        let t1 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "cancel-1");
        let t2 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "cancel-2");
        let t3 = mk(&mut s.db.conn, &s.unit_id, &s.cycle_id, "cancel-3");

        for t in [&t1, &t2, &t3] {
            update(
                &mut s.db.conn,
                &t.id,
                UpdateFields {
                    status: Some("cancelled".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "active",
            "plan must stay active when every task is merely cancelled"
        );
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(
            cycle.status, "active",
            "cycle must stay active (restartable) when every task is cancelled"
        );
    }

    // LM-11093 test helper: create a task under the given unit, attached to
    // `cycle_id` when Some. Backlog tasks are made by passing None.
    fn mk_task(conn: &mut Connection, unit_id: &str, cycle_id: Option<&str>, title: &str) -> Task {
        create(
            conn,
            CreateInput {
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
                cycle_id,
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
    }

    // LM-11093 (1/2): `blocked` must not hold a plan open. It means the work is
    // waiting on something outside this plan, so keeping the plan open cannot
    // resolve it. `repo::cycles::assert_no_todo_residue` has read it that way
    // since PDD-230; the plan gates disagreed, so a cycle could reach
    // `completed` while its own plan refused — citing a task that same cycle
    // had already accepted as terminal. This pins the agreement.
    #[test]
    fn cascade_completes_plan_with_blocked_task_still_in_cycle() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let blocked = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "waiting");
        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");

        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("upstream dependency".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:blocked-is-container-terminal".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "completed",
            "a blocked task waits on something outside the plan — it must not pin it open"
        );

        // The cycle closes with it. "A completed plan has no active cycle" is a
        // system invariant (`PLAN_HAS_ACTIVE_CYCLES` / PDD-231, DOGFOOD-004), so
        // the two arms have to move together — a cascade that closed only the
        // plan would produce a state the route path rejects.
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(
            cycle.status, "completed",
            "plan and cycle must reach completion together"
        );

        // The blocker itself is untouched — accepting it is not resolving it.
        assert_eq!(
            get(&s.db.conn, &blocked.id).unwrap().unwrap().status,
            "blocked"
        );
    }

    // LM-11093 (1/2, order): the same end state must close the plan regardless
    // of which transition arrives last. Blocking the final open task is the
    // natural ordering for the reported bug — you finish what you can, then
    // park the rest — and it is the path that exercises the cascade's entry
    // guards (dispatch at `update`, trigger check at `cascade_complete`). With
    // those on the narrow done/cancelled set the fix was order-dependent: `done` last
    // closed the plan, `blocked` last hung it forever.
    #[test]
    fn cascade_completes_plan_when_blocked_is_the_last_transition() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");
        let blocked = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "waiting");

        // Reverse of the previous test: finish first, block last.
        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:blocked-last".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("upstream dependency".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "completed",
            "blocking the last open task must dispatch the cascade, not skip it"
        );
        let cycle = cycles::get(&s.db.conn, &s.cycle_id).unwrap().unwrap();
        assert_eq!(cycle.status, "completed");
    }

    // LM-11093: the containers close together, so recovering a blocker is a
    // known, walkable path — not a dead end. This walks it end to end, because
    // the cost is real and a future change that raises it should fail here:
    // re-open the plan, create and activate a cycle, re-attach, then start.
    // The old cycle stays frozen (v3.0: completed cycles do not restart).
    #[test]
    fn blocked_task_is_recoverable_after_the_containers_auto_complete() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");
        let blocked = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "waiting");

        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:recovery".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("upstream dependency".into())),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "completed"
        );
        assert_eq!(
            cycles::get(&s.db.conn, &s.cycle_id)
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );

        // The blocker resolves. Step 1: re-open the plan.
        plans::update(
            &s.db.conn,
            &s.plan_id,
            plans::UpdateFields {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .expect("a completed plan must be re-openable");

        // Step 2-3: the old cycle is frozen, so stand up a new one.
        assert!(
            cycles::update(
                &s.db.conn,
                &s.cycle_id,
                cycles::UpdateFields {
                    status: Some("active".into()),
                    ..Default::default()
                },
            )
            .is_err(),
            "completed cycles do not restart — recovery goes through a new one"
        );
        let plan_for_cycle = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        let next = cycles::create(
            &s.db.conn,
            cycles::CreateInput {
                project_id: &plan_for_cycle.project_id,
                unit_id: &s.unit_id,
                title: "C2",
                goal: None,
                idx: None,
            },
        )
        .unwrap()
        .unwrap();
        cycles::activate(&s.db.conn, &next.id).unwrap();

        // Step 4: re-attach and resume.
        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                cycle_id: Some(Some(next.id.clone())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                status: Some("in_progress".into()),
                ..Default::default()
            },
        )
        .expect("the unblocked task must be startable once containers are live again");

        assert_eq!(
            get(&s.db.conn, &blocked.id).unwrap().unwrap().status,
            "in_progress"
        );
    }

    // LM-11093 (1/2, converse): `todo` / `in_progress` still hold the plan open.
    // Without this the fix reads as "nothing blocks completion any more".
    #[test]
    fn cascade_blocked_by_todo_task_in_cycle() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let _pending = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "not started");
        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");

        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:todo-still-blocks".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "active",
            "unstarted work is this plan's own — it must keep the plan open"
        );
    }

    // LM-11093 (2/2, order): deferring the last open task must close the plan,
    // just as finishing it would. A detach carries no status field, so a
    // status-only dispatch never fired for it — leaving the backlog axis with
    // the same order-dependence the `blocked` axis had: finish-then-defer hung
    // the plan open forever while defer-then-finish closed it, for the very same
    // end state. This drives the order that used to fail.
    #[test]
    fn cascade_completes_plan_when_deferral_is_the_last_transition() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");
        let deferred = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "next round");

        // Finish first — the plan cannot close yet, `deferred` is still todo.
        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:defer-last".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "active"
        );

        // Defer last. Nothing about the task's status changes — only its cycle.
        update(
            &mut s.db.conn,
            &deferred.id,
            UpdateFields {
                cycle_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "completed",
            "deferring the last open task must dispatch the cascade, not skip it"
        );
        assert_eq!(
            get(&s.db.conn, &deferred.id).unwrap().unwrap().status,
            "todo",
            "deferral is not a status change — the task is parked, not finished"
        );
        // Both arms or neither: a `completed` plan over an `active` cycle is the
        // state `routes::plans` rejects with `PLAN_HAS_ACTIVE_CYCLES` (PDD-231),
        // so the cascade must not produce it. The detached task cannot name the
        // cycle it left, which is why `update` hands the pre-update id down as
        // `left_cycle` — without that this arm is unreachable on this axis and
        // the assertion below fails while the plan one passes.
        assert_eq!(
            cycles::get(&s.db.conn, &s.cycle_id)
                .unwrap()
                .unwrap()
                .status,
            "completed",
            "the vacated cycle holds only done work — it must close with the plan"
        );
    }

    // A QA defect must keep its containers open even though `bulk_sync`
    // transcribes it as `blocked` (`routes::discover`, `"defect" => "blocked"`).
    // `blocked` is container-terminal on the premise that it waits on something
    // outside the plan; a defect is work *inside* it. Reading status alone, this
    // round (1 pass + 1 defect) is entirely `{done, blocked}` and both containers
    // close over an open defect — after which `PLAN_COMPLETED` rejects `create`
    // and the plan-active guard rejects `in_progress`, so the defect cannot be
    // fixed without reopening by hand.
    #[test]
    fn cascade_keeps_plan_open_when_a_qa_defect_is_blocked() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let passed = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario A");
        let defect = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario B");

        update(
            &mut s.db.conn,
            &passed.id,
            UpdateFields {
                status: Some("done".into()),
                qa_status: Some(Some("pass".into())),
                evidence: Some(Some("test:qa-pass".into())),
                ..Default::default()
            },
        )
        .unwrap();

        // Exactly what bulk_sync writes for a defect row: status `blocked`,
        // `qa_status` carrying the real verdict.
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                status: Some("blocked".into()),
                qa_status: Some(Some("defect".into())),
                blocked_reason: Some(Some("assertion failed on step 3".into())),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "active",
            "an open QA defect is work inside the plan — it must hold the plan open"
        );
        assert_eq!(
            cycles::get(&s.db.conn, &s.cycle_id)
                .unwrap()
                .unwrap()
                .status,
            "active",
            "the round is not finished while one of its scenarios reports a defect"
        );

        // And the manual gates must agree with the cascade — otherwise the two
        // completion paths write different databases, which is the defect class
        // `count_completion_residue` was extracted to prevent.
        assert_eq!(
            plans::count_completion_residue(&s.db.conn, &s.plan_id).unwrap(),
            1,
            "the plan residue gate must count the defect too"
        );
        let cycle_gate = cycles::update(
            &s.db.conn,
            &s.cycle_id,
            cycles::UpdateFields {
                status: Some("completed".into()),
                ..Default::default()
            },
        );
        assert!(
            cycle_gate.is_err(),
            "manual `cycle complete` must refuse a round with an open defect"
        );

        // Once the defect is resolved the containers close as usual — the guard
        // keys on the defect verdict, not on `blocked` in general.
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                status: Some("done".into()),
                qa_status: Some(Some("pass".into())),
                evidence: Some(Some("test:qa-refixed".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "completed",
            "with the defect resolved the plan completes normally"
        );
    }

    // An ordinary blocker — no QA verdict attached — keeps the PDD-230 behaviour:
    // it is external, unresolvable from inside the plan, so it does NOT hold the
    // containers open. This is the other half of the defect rule above; without
    // it a `qa_status` check could silently re-break what PDD-230 fixed.
    #[test]
    fn cascade_still_completes_over_a_plain_external_blocker() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");
        let waiting = mk_task(
            &mut s.db.conn,
            &s.unit_id,
            Some(&s.cycle_id),
            "waiting on ops",
        );

        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:plain-blocker".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &waiting.id,
            UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("waiting on an external team".into())),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            get(&s.db.conn, &waiting.id)
                .unwrap()
                .unwrap()
                .qa_status
                .is_none(),
            "precondition: a plain blocker carries no QA verdict"
        );
        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "completed",
            "an external blocker must not pin the plan open (PDD-230)"
        );
    }

    // A defect verdict does not survive cancellation. `update` writes `qa_status`
    // only when the patch carries it and `blocked → cancelled` is legal, so a
    // cancelled row can keep a stale `defect`. Keyed on the verdict alone that row
    // would be open work forever and the only way out of `cancelled` is back to
    // `todo` — the permanently-uncompletable plan this change set exists to remove.
    #[test]
    fn cascade_completes_when_a_defect_row_is_cancelled() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let passed = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario A");
        let defect = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario B");

        update(
            &mut s.db.conn,
            &passed.id,
            UpdateFields {
                status: Some("done".into()),
                qa_status: Some(Some("pass".into())),
                evidence: Some(Some("test:cancel-defect".into())),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                status: Some("blocked".into()),
                qa_status: Some(Some("defect".into())),
                blocked_reason: Some(Some("assertion failed".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "active",
            "precondition: the open defect holds the plan"
        );

        // Cancel it — the scenario is withdrawn, not fixed. The patch carries no
        // `qa_status`, so `defect` stays on the row.
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                status: Some("cancelled".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let cancelled = get(&s.db.conn, &defect.id).unwrap().unwrap();
        assert_eq!(cancelled.status, "cancelled");
        assert_eq!(
            cancelled.qa_status.as_deref(),
            Some("defect"),
            "precondition: the stale verdict is still on the row — that is the trap"
        );

        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "completed",
            "cancelled work is finished regardless of a stale QA verdict"
        );
        assert_eq!(
            plans::count_completion_residue(&s.db.conn, &s.plan_id).unwrap(),
            0,
            "the SQL gate must agree — otherwise the two paths diverge again"
        );
    }

    // Clearing the verdict is itself a settling transition. `routes::tasks` accepts
    // `qa_status` independently of `status`, so a lone `{"qa_status":"pass"}` patch
    // on the last blocked defect row settles the plan — and must dispatch the
    // cascade. A status-only dispatch would leave the container open until some
    // unrelated edit fired, which is the transition-order dependence the other two
    // dispatch axes were widened to remove.
    #[test]
    fn cascade_dispatches_when_a_defect_verdict_is_cleared_alone() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let passed = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario A");
        let defect = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "scenario B");

        update(
            &mut s.db.conn,
            &passed.id,
            UpdateFields {
                status: Some("done".into()),
                qa_status: Some(Some("pass".into())),
                evidence: Some(Some("test:clear-verdict".into())),
                ..Default::default()
            },
        )
        .unwrap();
        // Exactly what bulk_sync writes for a defect row.
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                status: Some("blocked".into()),
                qa_status: Some(Some("defect".into())),
                blocked_reason: Some(Some("assertion failed on step 3".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "active",
            "precondition: the blocked defect holds the plan"
        );

        // Verdict-only patch: no status field at all.
        update(
            &mut s.db.conn,
            &defect.id,
            UpdateFields {
                qa_status: Some(Some("pass".into())),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap().status,
            "completed",
            "clearing the last defect verdict must dispatch the cascade, not wait for an unrelated edit"
        );
    }

    // LM-11093 (2/2): a task detached from its cycle (`task update --cycle ""`)
    // is backlog — deferred to a later round, not this plan's remaining work.
    // `tasks.unit_id` is NOT NULL so the task keeps its unit and the plan filter
    // JOINs through units, which previously kept deferred work in the set.
    // Reproduces the production case: work shipped, one task deferred.
    #[test]
    fn cascade_ignores_backlog_tasks_when_completing_plan() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let deferred = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "next round");
        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");

        // `todo` — deliberately NOT blocked, so this test isolates deferral.
        // If it were blocked the previous test's rule would carry it anyway and
        // this one would pass for the wrong reason.
        update(
            &mut s.db.conn,
            &deferred.id,
            UpdateFields {
                cycle_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();

        let detached = get(&s.db.conn, &deferred.id).unwrap().unwrap();
        assert!(detached.cycle_id.is_none(), "task must land in the backlog");
        assert_eq!(
            detached.unit_id, s.unit_id,
            "unit_id is NOT NULL — it stays"
        );
        assert_eq!(detached.status, "todo", "deferral is not a status change");

        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:backlog-excluded".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "completed",
            "a deferred todo task must not keep the plan open once scheduled work is done"
        );
    }

    // LM-11093: the same task, left in the cycle, must still block — proving the
    // exclusion is keyed on the user's explicit act of detaching, not on status.
    #[test]
    fn cascade_blocked_by_same_task_when_left_in_cycle() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let _kept = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "next round");
        let shipped = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "shipped");

        update(
            &mut s.db.conn,
            &shipped.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:not-detached-still-blocks".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "active",
            "an identical task that was NOT detached must still hold the plan open"
        );
    }

    // LM-11093: a plan whose work is *entirely* deferred stays open. Nothing in
    // it was finished, so there is nothing to close on. The term that holds it
    // open is `any(done)` over the scheduled set — deferring every task empties
    // that set, so the requirement cannot be met. (A separate emptiness guard
    // was tried here and removed: `any(done)` already implies it, so the guard
    // could never decide the outcome.)
    #[test]
    fn cascade_leaves_plan_open_when_all_work_is_deferred() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let only = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "deferred");
        update(
            &mut s.db.conn,
            &only.id,
            UpdateFields {
                cycle_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        update(
            &mut s.db.conn,
            &only.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:all-deferred".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(
            plan.status, "active",
            "no scheduled work was completed — the plan has nothing to close on"
        );
    }

    // LM-11093: the cascade path is Rust-side; the residue gate in
    // `repo::plans::update` is a separate SQL predicate that must agree. Without
    // this, reverting the SQL would leave the suite green.
    #[test]
    fn plan_residue_gate_accepts_blocked_and_backlog_tasks() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let blocked = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "blocked");
        let deferred = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "deferred");

        update(
            &mut s.db.conn,
            &blocked.id,
            UpdateFields {
                status: Some("blocked".into()),
                blocked_reason: Some(Some("upstream".into())),
                ..Default::default()
            },
        )
        .unwrap();
        // Left as `todo` in the backlog — the status the old gate rejected.
        update(
            &mut s.db.conn,
            &deferred.id,
            UpdateFields {
                cycle_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();

        plans::update(
            &s.db.conn,
            &s.plan_id,
            plans::UpdateFields {
                status: Some("completed".into()),
                ..Default::default()
            },
        )
        .expect("blocked + backlog tasks must not count as completion residue");

        let plan = plans::get(&s.db.conn, &s.plan_id).unwrap().unwrap();
        assert_eq!(plan.status, "completed");
    }

    // LM-11093 (converse): the residue gate must still reject genuinely
    // unfinished scheduled work, with a message naming the real criterion.
    #[test]
    fn plan_residue_gate_rejects_todo_task_in_cycle() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();
        let _pending = mk_task(&mut s.db.conn, &s.unit_id, Some(&s.cycle_id), "not started");

        let err = plans::update(
            &s.db.conn,
            &s.plan_id,
            plans::UpdateFields {
                status: Some("completed".into()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("todo/in_progress"),
            "the gate must name what it actually rejects, got: {err}"
        );
    }

    // LM-11031: once cascade_complete promotes the plan to `completed`,
    // subsequent task creation under any of its units must fail with
    // PLAN_COMPLETED. Without this gate, a delayed `task create` after the
    // cascade would silently produce orphan TODO tasks under a Complete plan
    // — the lying-state bug observed in the web dashboard.
    #[test]
    fn blocks_task_create_under_completed_plan() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "first",
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

        update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:gate".into())),
                ..Default::default()
            },
        )
        .unwrap();

        // Plan is now completed via cascade. Creating a follow-up task must fail.
        let err = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "post-cascade",
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
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("PLAN_COMPLETED:"),
            "expected PLAN_COMPLETED gate, got: {msg}"
        );
    }

    // LM-11031: symmetric gate for `units::create` — once a plan is
    // completed, no new units may be attached. Tested here (rather than in
    // units.rs) to reuse the cascade-completing Scene from `setup(true)`.
    #[test]
    fn blocks_unit_create_under_completed_plan() {
        let mut s = setup(true);
        cycles::activate(&s.db.conn, &s.cycle_id).unwrap();

        let t = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title: "first",
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

        update(
            &mut s.db.conn,
            &t.id,
            UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some("test:gate".into())),
                ..Default::default()
            },
        )
        .unwrap();

        let err = units::create(
            &s.db.conn,
            units::CreateInput {
                plan_id: &s.plan_id,
                title: "post-cascade-unit",
                goal: None,
                idx: None,
                execution_mode: None,
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.starts_with("PLAN_COMPLETED:"),
            "expected PLAN_COMPLETED gate, got: {msg}"
        );
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

    // ---- RL-U3-05 / LM-140: envelope-only FTS search ----

    /// Create a task and attach an envelope. Body intentionally avoids the
    /// envelope's vocabulary so we can prove the search hit came from FTS5
    /// indexing the envelope, not the task body.
    fn task_with_envelope(s: &mut Scene, title: &str, body: &str, envelope_json: &str) -> String {
        let task = create(
            &mut s.db.conn,
            CreateInput {
                unit_id: &s.unit_id,
                title,
                body: Some(body),
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

        let env = crate::repo::task_envelopes::create(
            &s.db.conn,
            crate::repo::task_envelopes::CreateInput {
                task_id: &task.id,
                version: 1,
                json: envelope_json,
                signed_by: "test",
            },
        )
        .unwrap();
        crate::repo::task_envelopes::set_active_on_task(&s.db.conn, &task.id, &env.id).unwrap();
        task.id
    }

    #[test]
    fn envelope_mode_finds_match_when_body_does_not_contain_query() {
        let mut s = setup(true);
        let t_id = task_with_envelope(
            &mut s,
            "Title with no special words",
            "body with nothing relevant",
            r#"{
                "intent": "rollout the heliocentric switch",
                "prompt_template": "be careful around the gyroscope",
                "success_criteria": "all calibration runs converge"
            }"#,
        );

        let hits = keyword_search_envelope_only(&s.db.conn, "heliocentric", 10).unwrap();
        let ids: Vec<_> = hits.iter().map(|t| t.id.clone()).collect();
        assert!(
            ids.contains(&t_id),
            "envelope mode must hit task whose envelope.intent contains query, got {ids:?}"
        );

        // Default keyword mode (tasks_fts) must NOT hit — proves the envelope
        // index is the only path that surfaced this task.
        let body_hits = keyword_search(&s.db.conn, "heliocentric", 10).unwrap();
        assert!(
            !body_hits.iter().any(|t| t.id == t_id),
            "default keyword mode hit the task — envelope FTS isn't the only source"
        );
    }

    #[test]
    fn envelope_mode_searches_each_indexed_field() {
        let mut s = setup(true);
        let intent_match = task_with_envelope(
            &mut s,
            "T-intent",
            "filler",
            r#"{"intent": "alphazebra rollout", "prompt_template": "x", "success_criteria": "x"}"#,
        );
        let prompt_match = task_with_envelope(
            &mut s,
            "T-prompt",
            "filler",
            r#"{"intent": "x", "prompt_template": "betalion playbook", "success_criteria": "x"}"#,
        );
        let success_match = task_with_envelope(
            &mut s,
            "T-success",
            "filler",
            r#"{"intent": "x", "prompt_template": "x", "success_criteria": "gammaowl coverage 100%"}"#,
        );

        let by_intent = keyword_search_envelope_only(&s.db.conn, "alphazebra", 10).unwrap();
        assert!(by_intent.iter().any(|t| t.id == intent_match));

        let by_prompt = keyword_search_envelope_only(&s.db.conn, "betalion", 10).unwrap();
        assert!(by_prompt.iter().any(|t| t.id == prompt_match));

        let by_success = keyword_search_envelope_only(&s.db.conn, "gammaowl", 10).unwrap();
        assert!(by_success.iter().any(|t| t.id == success_match));
    }

    #[test]
    fn envelope_mode_skips_superseded_envelopes() {
        let mut s = setup(true);
        let t_id = task_with_envelope(
            &mut s,
            "Title",
            "body",
            r#"{"intent": "obsolete sigma rollout", "prompt_template": "", "success_criteria": ""}"#,
        );

        // Sign a v2 envelope that intentionally omits "sigma". sign_for_task
        // (the route helper) would mark v1 as superseded; here we replicate
        // the behavior via supersede + create.
        let active_v1 = crate::repo::task_envelopes::active_for_task(&s.db.conn, &t_id)
            .unwrap()
            .unwrap();
        crate::repo::task_envelopes::supersede(&s.db.conn, &active_v1.id, "ENV-NEXT-PLACEHOLDER")
            .ok();
        let v2 = crate::repo::task_envelopes::create(
            &s.db.conn,
            crate::repo::task_envelopes::CreateInput {
                task_id: &t_id,
                version: 2,
                json: r#"{"intent": "different topic", "prompt_template": "", "success_criteria": ""}"#,
                signed_by: "test",
            },
        )
        .unwrap();
        // Fix the placeholder back to the real superseder id so the index
        // reflects "v1 superseded by v2".
        s.db.conn
            .execute(
                "UPDATE task_envelopes SET superseded_by = ?1 WHERE id = ?2",
                params![v2.id, active_v1.id],
            )
            .unwrap();

        // The query that ONLY matches v1 (the superseded envelope) must
        // return nothing now — we exclude superseded rows.
        let hits = keyword_search_envelope_only(&s.db.conn, "sigma", 10).unwrap();
        assert!(
            !hits.iter().any(|t| t.id == t_id),
            "search must skip superseded envelope — got {:?}",
            hits.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn envelope_mode_empty_query_returns_empty() {
        let s = setup(true);
        let hits = keyword_search_envelope_only(&s.db.conn, "   ", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn envelope_mode_returns_empty_when_no_envelopes_indexed() {
        let s = setup(true);
        let hits = keyword_search_envelope_only(&s.db.conn, "anything", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn list_status_filter_accepts_comma_separated_values() {
        // LM-11092: `status` matches any of a comma-separated list of statuses.
        let mut s = setup(true);
        let mk = |conn: &mut rusqlite::Connection, unit: &str, title: &str| -> String {
            create(
                conn,
                CreateInput {
                    unit_id: unit,
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
        };
        let _t1 = mk(&mut s.db.conn, &s.unit_id, "T1");
        let _t2 = mk(&mut s.db.conn, &s.unit_id, "T2");
        let t3 = mk(&mut s.db.conn, &s.unit_id, "T3");
        // todo → cancelled is a valid transition that needs neither an active
        // cycle nor evidence, so it gives us a second status to filter on.
        update(
            &mut s.db.conn,
            &t3,
            UpdateFields {
                status: Some("cancelled".to_string()),
                ..UpdateFields::default()
            },
        )
        .unwrap();

        let count = |status: Option<&str>| -> usize {
            list(
                &s.db.conn,
                ListFilter {
                    unit_id: Some(&s.unit_id),
                    status,
                    ..Default::default()
                },
            )
            .unwrap()
            .len()
        };

        assert_eq!(count(Some("todo")), 2, "single status filters as before");
        assert_eq!(count(Some("cancelled")), 1);
        assert_eq!(
            count(Some("todo,cancelled")),
            3,
            "comma list matches any of the statuses"
        );
        assert_eq!(
            count(Some("todo, cancelled")),
            3,
            "surrounding whitespace in segments is trimmed"
        );
        assert_eq!(
            count(Some("todo,,")),
            2,
            "empty segments from stray commas are ignored"
        );
        assert_eq!(count(Some("")), 3, "an empty filter matches everything");
        assert_eq!(count(None), 3, "no status filter matches everything");
    }
}
