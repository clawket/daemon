use crate::id::{new_id, now_ms};
use crate::models::Run;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

pub struct CreateInput<'a> {
    pub task_id: &'a str,
    pub session_id: Option<&'a str>,
    pub agent: &'a str,
    pub status: Option<&'a str>,
    pub envelope_id: Option<&'a str>,
    pub envelope_snapshot: Option<&'a Value>,
}

pub fn create(conn: &Connection, input: CreateInput<'_>) -> Result<Option<Run>> {
    let id = new_id("RUN");
    let status = input.status.unwrap_or("started");
    let snapshot_json = input
        .envelope_snapshot
        .map(serde_json::to_string)
        .transpose()?;
    conn.execute(
        "INSERT INTO runs
            (id, task_id, session_id, agent, started_at, status, envelope_id, envelope_snapshot)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            input.task_id,
            input.session_id,
            input.agent,
            now_ms(),
            status,
            input.envelope_id,
            snapshot_json,
        ],
    )?;
    get(conn, &id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Run>> {
    let r = conn
        .query_row(
            "SELECT id, task_id, session_id, agent, started_at, ended_at, result, notes,
                    status, envelope_id, envelope_snapshot,
                    command, input_hash, exit_code, report_hash
             FROM runs WHERE id = ?1",
            params![id],
            map_run,
        )
        .optional()?;
    Ok(r)
}

#[derive(Default)]
pub struct ListFilter<'a> {
    pub task_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

pub fn list(conn: &Connection, filter: ListFilter<'_>) -> Result<Vec<Run>> {
    if let Some(pid) = filter.project_id {
        let mut stmt = conn.prepare(
            "SELECT r.id, r.task_id, r.session_id, r.agent, r.started_at, r.ended_at, r.result, r.notes,
                    r.status, r.envelope_id, r.envelope_snapshot,
                    r.command, r.input_hash, r.exit_code, r.report_hash
             FROM runs r
             JOIN tasks s ON s.id = r.task_id
             JOIN units u ON u.id = s.unit_id
             JOIN plans pl ON pl.id = u.plan_id
             WHERE pl.project_id = ?1
             ORDER BY r.started_at DESC",
        )?;
        let rows = stmt.query_map(params![pid], map_run)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        return Ok(out);
    }

    let mut sql = String::from(
        "SELECT id, task_id, session_id, agent, started_at, ended_at, result, notes,
                status, envelope_id, envelope_snapshot,
                command, input_hash, exit_code, report_hash
         FROM runs",
    );
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(v) = filter.task_id {
        clauses.push("task_id = ?");
        vals.push(v.to_string().into());
    }
    if let Some(v) = filter.session_id {
        clauses.push("session_id = ?");
        vals.push(v.to_string().into());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY started_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let params_iter = rusqlite::params_from_iter(vals.iter());
    let rows = stmt.query_map(params_iter, map_run)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn finish(
    conn: &Connection,
    id: &str,
    result: &str,
    notes: Option<&str>,
) -> Result<Option<Run>> {
    conn.execute(
        "UPDATE runs SET ended_at = ?1, result = ?2, notes = ?3, status = 'finished'
         WHERE id = ?4",
        params![now_ms(), result, notes, id],
    )?;
    get(conn, id)
}

pub fn mark_started(conn: &Connection, id: &str) -> Result<Option<Run>> {
    conn.execute(
        "UPDATE runs SET status = 'started' WHERE id = ?1 AND status = 'pending'",
        params![id],
    )?;
    get(conn, id)
}

/// Verification fields a run may carry (migration 029). Each is optional on
/// the wire; `set_verification` only overwrites the fields the caller supplied,
/// so `POST /runs` can record the command up front and `finish` can add the
/// exit code and report digest later.
#[derive(Default, Debug, Clone)]
pub struct Verification<'a> {
    pub command: Option<&'a str>,
    pub input_hash: Option<&'a str>,
    pub exit_code: Option<i64>,
    pub report_hash: Option<&'a str>,
}

impl Verification<'_> {
    pub fn is_empty(&self) -> bool {
        self.command.is_none()
            && self.input_hash.is_none()
            && self.exit_code.is_none()
            && self.report_hash.is_none()
    }
}

pub fn set_verification(conn: &Connection, id: &str, v: &Verification<'_>) -> Result<()> {
    if v.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE runs SET
            command     = COALESCE(?2, command),
            input_hash  = COALESCE(?3, input_hash),
            exit_code   = COALESCE(?4, exit_code),
            report_hash = COALESCE(?5, report_hash)
         WHERE id = ?1",
        params![id, v.command, v.input_hash, v.exit_code, v.report_hash],
    )?;
    Ok(())
}

/// Whether the evidence cites runs at all (any `run:<token>`). Callers use
/// this — not `evidence_run_refs` — to decide whether to validate, so a
/// malformed token such as `run:undefined` is rejected instead of passing as
/// free text.
pub fn cites_runs(evidence: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\brun:\S").unwrap())
        .is_match(evidence)
}

/// `run:<token>` tokens that are not a run id (`run:undefined`, `run:${ID}`).
fn malformed_run_refs(evidence: &str) -> Vec<String> {
    static TOKEN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let token = TOKEN.get_or_init(|| regex::Regex::new(r"\brun:(\S+)").unwrap());
    let id = ID.get_or_init(|| regex::Regex::new(r"^RUN-[A-Za-z0-9_-]+[.,;:)\]]*$").unwrap());
    token
        .captures_iter(evidence)
        .map(|c| c[1].to_string())
        .filter(|t| !id.is_match(t))
        .collect()
}

/// `run:<RUN-ID>` tokens cited in an evidence string, in order, deduplicated.
pub fn evidence_run_refs(evidence: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\brun:(RUN-[A-Za-z0-9_-]+)").unwrap());
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(evidence) {
        let id = cap[1].to_string();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Validate every `run:<ID>` reference in `evidence` against the runs table.
///
/// A reference is only evidence when it points at a finished run that recorded
/// what it executed (`command`), what it ran on (`input_hash`), how it ended
/// (`exit_code`) and what it reported (`report_hash`), and when that run
/// belongs to the same project as the task citing it. Anything less is a
/// pointer to nothing a reviewer can re-check, so it is rejected with a coded
/// error (`routes::error` maps all three to HTTP 400).
///
/// Evidence without any `run:` token is not judged here — free text stays a
/// valid form of evidence for non-test work.
pub fn validate_evidence_refs(
    conn: &Connection,
    evidence: &str,
    project_id: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(bad) = malformed_run_refs(evidence).first() {
        anyhow::bail!(
            "EVIDENCE_RUN_NOT_FOUND: evidence cites run:{bad}, which is not a run id (expected run:RUN-<id>)"
        );
    }
    let refs = evidence_run_refs(evidence);
    for rid in &refs {
        let run = get(conn, rid)?.ok_or_else(|| {
            anyhow::anyhow!(
                "EVIDENCE_RUN_NOT_FOUND: evidence cites run:{rid}, which does not exist"
            )
        })?;
        let mut missing: Vec<&str> = Vec::new();
        if run.status != "finished" {
            missing.push("status=finished");
        }
        if run
            .command
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            missing.push("command");
        }
        if run
            .input_hash
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            missing.push("input_hash");
        }
        if run.exit_code.is_none() {
            missing.push("exit_code");
        }
        if run
            .report_hash
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            missing.push("report_hash");
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "EVIDENCE_RUN_INCOMPLETE: run:{rid} is missing {} — finish the run with command, input_hash, exit_code and report_hash before citing it",
                missing.join(", ")
            );
        }
        if let Some(pid) = project_id {
            let run_project = task_project_id(conn, &run.task_id)?;
            if run_project.as_deref() != Some(pid) {
                anyhow::bail!(
                    "EVIDENCE_RUN_FOREIGN_PROJECT: run:{rid} belongs to project {}, not {pid}",
                    run_project.as_deref().unwrap_or("<none>")
                );
            }
        }
    }
    Ok(refs)
}

/// Project that owns a task (task → unit → plan → project).
pub fn task_project_id(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT pl.project_id FROM tasks t
             JOIN units u ON u.id = t.unit_id
             JOIN plans pl ON pl.id = u.plan_id
             WHERE t.id = ?1",
            params![task_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

fn map_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let snapshot_str: Option<String> = r.get(10)?;
    let envelope_snapshot = snapshot_str
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    Ok(Run {
        id: r.get(0)?,
        task_id: r.get(1)?,
        session_id: r.get(2)?,
        agent: r.get(3)?,
        started_at: r.get(4)?,
        ended_at: r.get(5)?,
        result: r.get(6)?,
        notes: r.get(7)?,
        status: r.get(8)?,
        envelope_id: r.get(9)?,
        envelope_snapshot,
        command: r.get(11)?,
        input_hash: r.get(12)?,
        exit_code: r.get(13)?,
        report_hash: r.get(14)?,
    })
}

#[cfg(test)]
mod evidence_ref_tests {
    use super::*;
    use crate::repo::tasks;
    use crate::test_fixtures::{fixture, scope, task, Fx, Scope};

    fn run_for(fx: &Fx, task_id: &str, v: Verification<'_>, finish_it: bool) -> String {
        let c = fx.state.conn();
        let r = create(
            &c,
            CreateInput {
                task_id,
                session_id: None,
                agent: "main",
                status: None,
                envelope_id: None,
                envelope_snapshot: None,
            },
        )
        .unwrap()
        .unwrap();
        set_verification(&c, &r.id, &v).unwrap();
        if finish_it {
            finish(&c, &r.id, "success", None).unwrap();
        }
        r.id
    }

    fn complete() -> Verification<'static> {
        Verification {
            command: Some("cargo test"),
            input_hash: Some("sha256:in"),
            exit_code: Some(0),
            report_hash: Some("sha256:report"),
        }
    }

    fn done_with(fx: &Fx, task_id: &str, evidence: &str) -> anyhow::Result<()> {
        let mut c = fx.state.conn();
        tasks::update(
            &mut c,
            task_id,
            tasks::UpdateFields {
                status: Some("done".into()),
                evidence: Some(Some(evidence.into())),
                ..Default::default()
            },
        )
        .map(|_| ())
    }

    fn setup() -> (Fx, Scope, Scope, String, String) {
        let fx = fixture();
        let a = scope(&fx, "Alpha", Some("/ea"), "A plan");
        let b = scope(&fx, "Beta", Some("/eb"), "B plan");
        let (ta, tb) = {
            let mut c = fx.state.conn();
            (
                task(&mut c, &a, "a", None, "in_progress"),
                task(&mut c, &b, "b", None, "in_progress"),
            )
        };
        (fx, a, b, ta, tb)
    }

    #[test]
    fn parses_run_tokens_once_each() {
        assert_eq!(
            evidence_run_refs("see run:RUN-abc and run:RUN-abc, plus run:RUN-x_1; prerun:RUN-no"),
            vec!["RUN-abc".to_string(), "RUN-x_1".to_string()]
        );
        assert!(evidence_run_refs("src/app.rs:42 summary").is_empty());
        assert!(cites_runs("run:undefined"));
        assert!(!cites_runs("src/app.rs:42 summary"));
        assert_eq!(
            malformed_run_refs("run:undefined and run:RUN-1, ok"),
            vec!["undefined".to_string()]
        );
        assert!(malformed_run_refs("(run:RUN-1) run:RUN-2.").is_empty());
    }

    #[test]
    fn valid_run_reference_completes_the_task() {
        let (fx, _a, _b, ta, _tb) = setup();
        let rid = run_for(&fx, &ta, complete(), true);
        done_with(&fx, &ta, &format!("run:{rid}")).unwrap();
    }

    #[test]
    fn missing_run_is_rejected() {
        let (fx, _a, _b, ta, _tb) = setup();
        let err = done_with(&fx, &ta, "run:RUN-doesnotexist")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("EVIDENCE_RUN_NOT_FOUND:"), "{err}");
    }

    /// `run:undefined` (an unexpanded template) must not pass as free text.
    #[test]
    fn malformed_run_reference_is_rejected() {
        let (fx, _a, _b, ta, _tb) = setup();
        let err = done_with(&fx, &ta, "tests pass: run:undefined")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("EVIDENCE_RUN_NOT_FOUND:"), "{err}");
        assert!(err.contains("not a run id"), "{err}");
    }

    #[test]
    fn incomplete_or_unfinished_run_is_rejected() {
        let (fx, _a, _b, ta, _tb) = setup();
        let partial = run_for(
            &fx,
            &ta,
            Verification {
                command: Some("npm test"),
                ..Default::default()
            },
            true,
        );
        let err = done_with(&fx, &ta, &format!("run:{partial}"))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("EVIDENCE_RUN_INCOMPLETE:"), "{err}");
        assert!(
            err.contains("input_hash") && err.contains("exit_code") && err.contains("report_hash"),
            "{err}"
        );
        let open = run_for(&fx, &ta, complete(), false);
        let err = done_with(&fx, &ta, &format!("run:{open}"))
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("EVIDENCE_RUN_INCOMPLETE:") && err.contains("status=finished"),
            "{err}"
        );
    }

    #[test]
    fn run_from_another_project_is_rejected() {
        let (fx, _a, _b, ta, tb) = setup();
        let foreign = run_for(&fx, &tb, complete(), true);
        let err = done_with(&fx, &ta, &format!("run:{foreign}"))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("EVIDENCE_RUN_FOREIGN_PROJECT:"), "{err}");
    }

    #[test]
    fn free_text_evidence_is_still_accepted() {
        let (fx, _a, _b, ta, _tb) = setup();
        done_with(&fx, &ta, "src/routes/gate.rs:40 reviewed the scoping query").unwrap();
    }

    #[test]
    fn error_codes_map_to_http_400() {
        use crate::routes::error::ApiError;
        for code in [
            "EVIDENCE_RUN_NOT_FOUND",
            "EVIDENCE_RUN_INCOMPLETE",
            "EVIDENCE_RUN_FOREIGN_PROJECT",
        ] {
            let api: ApiError = anyhow::anyhow!("{code}: x").into();
            assert_eq!(api.status, axum::http::StatusCode::BAD_REQUEST, "{code}");
            assert_eq!(api.code.as_deref(), Some(code));
        }
    }
}
