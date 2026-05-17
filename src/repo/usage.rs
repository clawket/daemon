//! task_usage repo (RL-U3-12 / LM-65, ADR-0007 enforcement).
//!
//! Records per-run input/output/cache token counts and USD cost. Provides
//! aggregate queries for the routes layer:
//!   - `record`         — UPSERT a run's usage row
//!   - `list_by_task`   — chronological per-run list for one task
//!   - `task_total`     — `Totals` summed across all runs of one task
//!   - `aggregate_by_plan` — per-model `ModelAggregate` across the plan
//!
//! Budget evaluation lives in routes/usage.rs where it has access to the
//! envelope's `token_budget` field via the existing resolve_chain helper.

use crate::id::now_ms;
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub run_id: String,
    pub task_id: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub token_budget_snapshot: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Totals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub run_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelAggregate {
    pub model: String,
    #[serde(flatten)]
    pub totals: Totals,
}

pub struct RecordInput<'a> {
    pub run_id: &'a str,
    pub task_id: &'a str,
    pub model: &'a str,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: f64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub token_budget_snapshot: Option<&'a str>,
}

/// UPSERT a usage row keyed on `run_id`. Re-recording the same run
/// replaces prior numbers (later refinements of token counts win).
pub fn record(conn: &Connection, input: RecordInput<'_>) -> Result<UsageRecord> {
    let started_at = input.started_at.unwrap_or_else(now_ms);
    conn.execute(
        "INSERT INTO task_usage (
            run_id, task_id, model,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            cost_usd, started_at, ended_at, token_budget_snapshot
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(run_id) DO UPDATE SET
            task_id = excluded.task_id,
            model = excluded.model,
            input_tokens = excluded.input_tokens,
            output_tokens = excluded.output_tokens,
            cache_read_tokens = excluded.cache_read_tokens,
            cache_write_tokens = excluded.cache_write_tokens,
            cost_usd = excluded.cost_usd,
            ended_at = excluded.ended_at,
            token_budget_snapshot = excluded.token_budget_snapshot",
        params![
            input.run_id,
            input.task_id,
            input.model,
            input.input_tokens,
            input.output_tokens,
            input.cache_read_tokens,
            input.cache_write_tokens,
            input.cost_usd,
            started_at,
            input.ended_at,
            input.token_budget_snapshot,
        ],
    )?;
    Ok(UsageRecord {
        run_id: input.run_id.into(),
        task_id: input.task_id.into(),
        model: input.model.into(),
        input_tokens: input.input_tokens,
        output_tokens: input.output_tokens,
        cache_read_tokens: input.cache_read_tokens,
        cache_write_tokens: input.cache_write_tokens,
        cost_usd: input.cost_usd,
        started_at,
        ended_at: input.ended_at,
        token_budget_snapshot: input.token_budget_snapshot.map(str::to_owned),
    })
}

pub fn list_by_task(conn: &Connection, task_id: &str) -> Result<Vec<UsageRecord>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, task_id, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, cost_usd,
                started_at, ended_at, token_budget_snapshot
         FROM task_usage WHERE task_id = ?1 ORDER BY started_at ASC",
    )?;
    let rows = stmt.query_map(params![task_id], map_record)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn task_total(conn: &Connection, task_id: &str) -> Result<Totals> {
    let row = conn.query_row(
        "SELECT
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_read_tokens), 0),
            COALESCE(SUM(cache_write_tokens), 0),
            COALESCE(SUM(cost_usd), 0.0),
            COUNT(*)
         FROM task_usage WHERE task_id = ?1",
        params![task_id],
        |r| {
            Ok(Totals {
                input_tokens: r.get(0)?,
                output_tokens: r.get(1)?,
                cache_read_tokens: r.get(2)?,
                cache_write_tokens: r.get(3)?,
                cost_usd: r.get(4)?,
                run_count: r.get(5)?,
            })
        },
    )?;
    Ok(row)
}

/// Aggregate spend across every task whose unit belongs to `plan_id`,
/// grouped by model. Empty Vec when the plan has no recorded usage.
pub fn aggregate_by_plan(conn: &Connection, plan_id: &str) -> Result<Vec<ModelAggregate>> {
    let mut stmt = conn.prepare(
        "SELECT u.model,
                COALESCE(SUM(u.input_tokens), 0),
                COALESCE(SUM(u.output_tokens), 0),
                COALESCE(SUM(u.cache_read_tokens), 0),
                COALESCE(SUM(u.cache_write_tokens), 0),
                COALESCE(SUM(u.cost_usd), 0.0),
                COUNT(*)
         FROM task_usage u
         JOIN tasks t ON t.id = u.task_id
         JOIN units un ON un.id = t.unit_id
         WHERE un.plan_id = ?1
         GROUP BY u.model
         ORDER BY u.model ASC",
    )?;
    let rows = stmt.query_map(params![plan_id], |r| {
        Ok(ModelAggregate {
            model: r.get::<_, String>(0)?,
            totals: Totals {
                input_tokens: r.get(1)?,
                output_tokens: r.get(2)?,
                cache_read_tokens: r.get(3)?,
                cache_write_tokens: r.get(4)?,
                cost_usd: r.get(5)?,
                run_count: r.get(6)?,
            },
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn map_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    Ok(UsageRecord {
        run_id: r.get(0)?,
        task_id: r.get(1)?,
        model: r.get(2)?,
        input_tokens: r.get(3)?,
        output_tokens: r.get(4)?,
        cache_read_tokens: r.get(5)?,
        cache_write_tokens: r.get(6)?,
        cost_usd: r.get(7)?,
        started_at: r.get(8)?,
        ended_at: r.get(9)?,
        token_budget_snapshot: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, runs, tasks, units};

    struct Fx {
        _dir: tempfile::TempDir,
        db: Db,
        plan_id: String,
        unit_id: String,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
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
        Fx {
            _dir: dir,
            db,
            plan_id: plan.id,
            unit_id: unit.id,
        }
    }

    fn make_task(f: &mut Fx, title: &str) -> String {
        tasks::create(
            &mut f.db.conn,
            tasks::CreateInput {
                unit_id: &f.unit_id,
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

    fn make_run(f: &Fx, task_id: &str) -> String {
        runs::create(
            &f.db.conn,
            runs::CreateInput {
                task_id,
                session_id: None,
                agent: "main",
                status: Some("started"),
                envelope_id: None,
                envelope_snapshot: None,
            },
        )
        .unwrap()
        .unwrap()
        .id
    }

    #[test]
    fn record_and_list_by_task_round_trip() {
        let mut f = fx();
        let t = make_task(&mut f, "T");
        let r = make_run(&f, &t);
        record(
            &f.db.conn,
            RecordInput {
                run_id: &r,
                task_id: &t,
                model: "claude-opus-4-7",
                input_tokens: 1000,
                output_tokens: 2000,
                cache_read_tokens: 500,
                cache_write_tokens: 100,
                cost_usd: 1.23,
                started_at: Some(1000),
                ended_at: Some(2000),
                token_budget_snapshot: Some(r#"{"input_tokens":100000}"#),
            },
        )
        .unwrap();
        let rows = list_by_task(&f.db.conn, &t).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 1000);
        assert_eq!(rows[0].cost_usd, 1.23);
        assert_eq!(rows[0].model, "claude-opus-4-7");
        assert_eq!(rows[0].started_at, 1000);
    }

    #[test]
    fn record_upserts_on_duplicate_run_id() {
        let mut f = fx();
        let t = make_task(&mut f, "T");
        let r = make_run(&f, &t);
        record(
            &f.db.conn,
            RecordInput {
                run_id: &r,
                task_id: &t,
                model: "claude-haiku-4-5-20251001",
                input_tokens: 100,
                output_tokens: 200,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.10,
                started_at: Some(1),
                ended_at: None,
                token_budget_snapshot: None,
            },
        )
        .unwrap();
        record(
            &f.db.conn,
            RecordInput {
                run_id: &r,
                task_id: &t,
                model: "claude-haiku-4-5-20251001",
                input_tokens: 150, // updated
                output_tokens: 250,
                cache_read_tokens: 10,
                cache_write_tokens: 5,
                cost_usd: 0.20,
                started_at: Some(1),
                ended_at: Some(99),
                token_budget_snapshot: None,
            },
        )
        .unwrap();
        let rows = list_by_task(&f.db.conn, &t).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 150);
        assert_eq!(rows[0].ended_at, Some(99));
    }

    #[test]
    fn task_total_sums_all_runs() {
        let mut f = fx();
        let t = make_task(&mut f, "T");
        let r1 = make_run(&f, &t);
        let r2 = make_run(&f, &t);
        record(
            &f.db.conn,
            RecordInput {
                run_id: &r1,
                task_id: &t,
                model: "x",
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.5,
                started_at: Some(1),
                ended_at: Some(2),
                token_budget_snapshot: None,
            },
        )
        .unwrap();
        record(
            &f.db.conn,
            RecordInput {
                run_id: &r2,
                task_id: &t,
                model: "x",
                input_tokens: 200,
                output_tokens: 75,
                cache_read_tokens: 10,
                cache_write_tokens: 0,
                cost_usd: 1.0,
                started_at: Some(3),
                ended_at: None,
                token_budget_snapshot: None,
            },
        )
        .unwrap();
        let totals = task_total(&f.db.conn, &t).unwrap();
        assert_eq!(totals.input_tokens, 300);
        assert_eq!(totals.output_tokens, 125);
        assert_eq!(totals.run_count, 2);
        assert!((totals.cost_usd - 1.5).abs() < 1e-9);
    }

    #[test]
    fn task_total_zero_when_no_records() {
        let mut f = fx();
        let t = make_task(&mut f, "T");
        let totals = task_total(&f.db.conn, &t).unwrap();
        assert_eq!(totals.run_count, 0);
        assert_eq!(totals.input_tokens, 0);
        assert_eq!(totals.cost_usd, 0.0);
    }

    #[test]
    fn aggregate_by_plan_groups_by_model_across_units_and_tasks() {
        let mut f = fx();
        let t1 = make_task(&mut f, "T1");
        let t2 = make_task(&mut f, "T2");
        let r1a = make_run(&f, &t1);
        let r1b = make_run(&f, &t1);
        let r2 = make_run(&f, &t2);
        for (run, model, in_t) in &[
            (&r1a, "opus", 100i64),
            (&r1b, "haiku", 50i64),
            (&r2, "opus", 200i64),
        ] {
            record(
                &f.db.conn,
                RecordInput {
                    run_id: run,
                    task_id: if **run == r2 { &t2 } else { &t1 },
                    model,
                    input_tokens: *in_t,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_usd: 0.0,
                    started_at: Some(1),
                    ended_at: None,
                    token_budget_snapshot: None,
                },
            )
            .unwrap();
        }
        let agg = aggregate_by_plan(&f.db.conn, &f.plan_id).unwrap();
        assert_eq!(agg.len(), 2);
        let opus = agg.iter().find(|a| a.model == "opus").unwrap();
        let haiku = agg.iter().find(|a| a.model == "haiku").unwrap();
        assert_eq!(opus.totals.input_tokens, 300);
        assert_eq!(opus.totals.run_count, 2);
        assert_eq!(haiku.totals.input_tokens, 50);
        assert_eq!(haiku.totals.run_count, 1);
    }

    #[test]
    fn aggregate_by_plan_empty_when_plan_has_no_usage() {
        let f = fx();
        let agg = aggregate_by_plan(&f.db.conn, &f.plan_id).unwrap();
        assert!(agg.is_empty());
    }
}
