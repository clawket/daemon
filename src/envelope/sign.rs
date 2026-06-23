//! Application service for envelope signing — single funnel through
//! which every envelope mutation (POST /tasks, PATCH /tasks/:id,
//! create_subtask, plan import) MUST pass.
//!
//! Bridges the structural validator (`envelope::validate`) and the
//! persistence layer (`repo::task_envelopes::sign_for_task`). Web
//! warns on `Severity::Error` violations via
//! `POST /tasks/:id/envelope/validate`; if a mutation path bypassed
//! the same validator, the web warning would describe a contract the
//! daemon did not actually enforce ("lying UI"). Routing every
//! signer through this entry makes that mode structurally impossible
//! and keeps the lockstep invariant declared at the top of
//! `envelope::validate` honest.
//!
//! Strictness: we run `validate_envelope(strict=false)`. The strict
//! axis only adds *advisory warnings* for recommended-but-missing
//! fields (e.g. `target_model`, `max_turns`); blocking on them at
//! sign-time would break the documented advisory semantics. Only
//! `Severity::Error` aborts the sign.

use rusqlite::{Connection, Transaction};
use serde_json::Value;

use crate::envelope::validate::{validate_envelope, Severity, Violation};
use crate::models::TaskEnvelope;
use crate::repo::{task_envelopes, tasks};

/// Same depth bound used by the read-side `resolve_envelope` in
/// `routes::tasks`. Cycles are already rejected at task creation, so the
/// chain depth equals tree depth in practice; this is a paranoid cap.
const RESOLVE_CHAIN_MAX_DEPTH: i64 = 1024;

#[derive(Debug)]
pub enum EnvelopeSignError {
    /// At least one `Severity::Error` violation reported by
    /// `validate_envelope`. The full list is carried so the routes
    /// layer can surface it in the 400 response `details` payload
    /// and the web form can render per-field inline errors.
    Validation(Vec<Violation>),
    /// Anything raised by the persistence layer (rusqlite, JSON
    /// serialization, transaction conflict). Bubbled up unchanged so
    /// the existing `ApiError` mappers in `routes::error` handle it.
    Storage(anyhow::Error),
}

impl std::fmt::Display for EnvelopeSignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(violations) => {
                let summary = violations
                    .iter()
                    .map(|v| format!("{}: {}", v.field, v.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(f, "ENVELOPE_INVALID: {summary}")
            }
            Self::Storage(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for EnvelopeSignError {}

impl From<rusqlite::Error> for EnvelopeSignError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Storage(anyhow::Error::from(err))
    }
}

impl From<serde_json::Error> for EnvelopeSignError {
    fn from(err: serde_json::Error) -> Self {
        Self::Storage(anyhow::Error::from(err))
    }
}

impl From<anyhow::Error> for EnvelopeSignError {
    fn from(err: anyhow::Error) -> Self {
        Self::Storage(err)
    }
}

/// Validate `value` against the required tier and persist a new
/// envelope version for `task_id`. Returns `EnvelopeSignError::Validation`
/// with the full violation list if any `Severity::Error` is present;
/// otherwise delegates to `repo::task_envelopes::sign_for_task`.
pub fn sign_envelope(
    conn: &mut Connection,
    task_id: &str,
    value: &Value,
    signed_by: &str,
) -> Result<TaskEnvelope, EnvelopeSignError> {
    let tx = conn.transaction()?;
    let env = sign_envelope_in_tx(&tx, task_id, value, signed_by)?;
    tx.commit()?;
    Ok(env)
}

/// Tx-aware variant of [`sign_envelope`]: validates + persists the new
/// envelope inside the caller-owned transaction WITHOUT committing, so a
/// route handler can wrap `tasks::create_in_tx` + `sign_envelope_in_tx` in
/// one transaction (D2 atomicity: a rejected envelope leaves no orphan task).
pub fn sign_envelope_in_tx(
    conn: &Transaction,
    task_id: &str,
    value: &Value,
    signed_by: &str,
) -> Result<TaskEnvelope, EnvelopeSignError> {
    let json = serde_json::to_string(value)?;
    sign_envelope_from_json_in_tx(conn, task_id, value, &json, signed_by)
}

/// Variant for callers that have an already-serialized JSON string with
/// caller-controlled key ordering (`import_plan` preserves the source
/// file's bullet order via `Vec<(String, Value)>` — re-serializing from
/// `Value` would alphabetize because the default `serde_json::Map` is
/// `BTreeMap` without the `preserve_order` feature). The Value is used
/// only for validation; the raw `json` is what gets persisted.
///
/// The caller is responsible for ensuring `value` and `json` describe
/// the same envelope; passing a mismatched pair would break the lockstep
/// invariant by validating one shape and persisting another.
pub fn sign_envelope_from_json(
    conn: &mut Connection,
    task_id: &str,
    value: &Value,
    json: &str,
    signed_by: &str,
) -> Result<TaskEnvelope, EnvelopeSignError> {
    let tx = conn.transaction()?;
    let env = sign_envelope_from_json_in_tx(&tx, task_id, value, json, signed_by)?;
    tx.commit()?;
    Ok(env)
}

/// Tx-aware variant of [`sign_envelope_from_json`]: validates + persists
/// inside the caller-owned transaction WITHOUT committing (D2 atomicity).
pub fn sign_envelope_from_json_in_tx(
    conn: &Transaction,
    task_id: &str,
    value: &Value,
    json: &str,
    signed_by: &str,
) -> Result<TaskEnvelope, EnvelopeSignError> {
    // Validation runs on the *resolved* envelope, not the raw payload.
    // ADR-0001 M0 inheritance lets a child task store a sparse delta
    // (e.g. just `{"retry_policy": {...}}`) whose required-tier fields
    // are inherited from the parent chain — without this resolution
    // step the required-tier check would falsely reject every legal
    // child override. The read-side `routes::tasks::resolve_envelope`
    // and `runs` snapshot freeze use the same composition; keeping them
    // in lockstep is what makes the validator's contract honest.
    let resolved = resolve_with_payload(conn, task_id, value)?;
    let result = validate_envelope(&resolved, false);
    if !result.valid {
        let errors: Vec<Violation> = result
            .violations
            .into_iter()
            .filter(|v| matches!(v.severity, Severity::Error))
            .collect();
        return Err(EnvelopeSignError::Validation(errors));
    }
    task_envelopes::sign_for_task_in_tx(conn, task_id, json, signed_by)
        .map_err(EnvelopeSignError::from)
}

/// Compose the resolved envelope that *would* exist after persisting
/// `payload` as `task_id`'s new active envelope.
///
/// We walk the parent chain via `task_envelopes::resolve_chain`, which
/// returns entries in leaf → root order (the task itself first, parents
/// after). We deep-merge in root → leaf order (parent first, then each
/// child override, then the new payload last). The current task's own
/// existing active envelope is intentionally excluded — `payload` is
/// what replaces it.
fn resolve_with_payload(
    conn: &Connection,
    task_id: &str,
    payload: &Value,
) -> Result<Value, EnvelopeSignError> {
    // Read parent_task_id directly so we don't depend on the route
    // layer having pre-fetched the Task row.
    let parent_id: Option<String> = match tasks::get(conn, task_id)? {
        Some(t) => t.parent_task_id,
        None => None,
    };

    let mut acc = Value::Object(Default::default());
    if let Some(pid) = parent_id {
        let chain = task_envelopes::resolve_chain(conn, &pid, RESOLVE_CHAIN_MAX_DEPTH)?;
        // resolve_chain returns leaf → root (ORDER BY depth DESC); we
        // want root → leaf for deep-merge composition.
        for entry in chain.iter().rev() {
            if let Some(j) = &entry.json {
                if let Ok(v) = serde_json::from_str::<Value>(j) {
                    deep_merge(&mut acc, &v);
                }
            }
        }
    }
    deep_merge(&mut acc, payload);
    Ok(acc)
}

/// Recursive object-only deep merge mirroring
/// `routes::tasks::deep_merge` and `repo::task_envelopes::deep_merge`:
/// patch wins on scalar/array/null conflicts; nested objects merge
/// key-by-key. The three copies exist because each layer owns its own
/// types — the behavior is intentionally identical.
fn deep_merge(into: &mut Value, patch: &Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{plans, projects, tasks, units};
    use serde_json::json;

    fn setup_task() -> (tempfile::TempDir, Db, String) {
        // Minimal project/plan/unit/task chain so envelope sign has a
        // parent row to attach to. Mirrors the fixture pattern used in
        // repo::task_envelopes tests.
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
                auto_advance: false,
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
    fn sign_rejects_envelope_missing_required_tier() {
        let (_d, mut db, task_id) = setup_task();

        // Empty envelope — every required-tier field is missing.
        let result = sign_envelope(&mut db.conn, &task_id, &json!({}), "tester");
        match result {
            Err(EnvelopeSignError::Validation(violations)) => {
                let fields: Vec<_> = violations.iter().map(|v| v.field.as_str()).collect();
                assert!(fields.contains(&"intent"), "violations: {fields:?}");
                assert!(
                    fields.contains(&"prompt_template"),
                    "violations: {fields:?}"
                );
                assert!(
                    fields.contains(&"success_criteria"),
                    "violations: {fields:?}"
                );
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn sign_persists_envelope_when_required_tier_present() {
        let (_d, mut db, task_id) = setup_task();

        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
        });
        let signed = sign_envelope(&mut db.conn, &task_id, &env, "tester")
            .expect("expected successful sign");
        assert_eq!(signed.task_id, task_id);
        assert_eq!(signed.version, 1);
    }
}
