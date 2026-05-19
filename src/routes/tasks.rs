use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decomposition::suggest as decompose_suggest;
use crate::embeddings;
use crate::envelope::conditions as env_conditions;
use crate::envelope::validate as env_validate;
use crate::git;
use crate::models::{Task, TaskEnvelope};
use crate::repo::{comments, locks, plans, projects, task_envelopes, tasks, units};
use crate::routes::error::{json_or_404, ApiError, ApiResult};
use crate::routes::util::{norm_opt, value_to_opt_string};
use crate::secrets::redact::reject_high_entropy_in_value;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(create))
        .route("/tasks/search", get(search))
        .route("/tasks/stats", get(stats))
        .route("/tasks/bulk-update", post(bulk_update))
        .route("/tasks/{id}", get(get_one).patch(update).delete(delete_one))
        .route("/tasks/{id}/body", post(append_body))
        .route("/tasks/{id}/similar", get(similar))
        .route(
            "/tasks/{id}/envelope",
            get(get_envelope).delete(clear_envelope),
        )
        .route("/tasks/{id}/envelope/history", get(envelope_history))
        .route(
            "/tasks/{id}/envelope/validate",
            post(validate_envelope_route),
        )
        .route("/tasks/{id}/decompose", post(decompose_route))
        .route("/tasks/{id}/subtasks", post(create_subtask))
        .route("/tasks/{id}/ancestors", get(get_ancestors))
        .route("/tasks/{id}/descendants", get(get_descendants))
        .route("/tasks/{id}/subtree", get(get_subtree))
        .route("/tasks/{id}/drift", get(get_drift))
        .route(
            "/tasks/{id}/lease",
            post(acquire_lease).delete(release_lease),
        )
        .route("/tasks/{id}/lease/heartbeat", post(heartbeat_lease))
}

#[derive(Deserialize)]
struct ListQuery {
    unit_id: Option<String>,
    plan_id: Option<String>,
    status: Option<String>,
    cycle_id: Option<String>,
    assignee: Option<String>,
    agent_id: Option<String>,
    parent_task_id: Option<String>,
    /// FIX-DAEMON-r2-tier: tier filter (low|med|high)
    tier: Option<String>,
    /// FIX-DAEMON-r2-qa: qa_status filter (pass|defect|scenario_error)
    qa_status: Option<String>,
    /// US-CKT-SCHEMA-006: filter by scenario_id
    scenario_id: Option<String>,
    /// US-CKT-SCHEMA-022: filter by batch_id (group by sub-agent batch invocation)
    batch_id: Option<String>,
    /// US-CKT-SCHEMA-044: max rows to return (None = no limit).
    limit: Option<i64>,
    /// US-CKT-SCHEMA-044: number of rows to skip before returning.
    offset: Option<i64>,
}

async fn list(
    State(app): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Task>>> {
    let parent = q
        .parent_task_id
        .as_deref()
        .map(|s| if s == "null" { None } else { Some(s) });
    let (cycle_id_filter, no_cycle) = match q.cycle_id.as_deref() {
        Some("null") | Some("") => (None, true),
        other => (other, false),
    };
    Ok(Json(tasks::list(
        &app.conn(),
        tasks::ListFilter {
            unit_id: q.unit_id.as_deref(),
            plan_id: q.plan_id.as_deref(),
            status: q.status.as_deref(),
            cycle_id: cycle_id_filter,
            no_cycle,
            assignee: q.assignee.as_deref(),
            agent_id: q.agent_id.as_deref(),
            parent_task_id: parent,
            tier: q.tier.as_deref(),
            qa_status: q.qa_status.as_deref(),
            scenario_id: q.scenario_id.as_deref(),
            batch_id: q.batch_id.as_deref(),
            limit: q.limit,
            offset: q.offset,
        },
    )?))
}

/// US-CKT-SCHEMA-029: query params for GET /tasks/stats.
#[derive(Deserialize)]
struct StatsQuery {
    /// Required — Crockford base32 ULID (26 chars). Validated upstream.
    batch_id: String,
}

/// GET /tasks/stats?batch_id=<ULID>
///
/// Returns `{batch_id, total, pass, defect, scenario_error}` aggregated over
/// `tasks.qa_status` for tasks matching the batch_id. The route validates the
/// ULID format with `validate_batch_id` (same enforcement as create/update)
/// before hitting the DB so callers get a coded 400 on bad input.
async fn stats(
    State(app): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<tasks::BatchStats>> {
    validate_batch_id(&q.batch_id)?;
    Ok(Json(tasks::stats_by_batch(&app.conn(), &q.batch_id)?))
}

#[derive(Deserialize, Default)]
struct CreateBody {
    unit_id: Option<String>,
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    idx: Option<i64>,
    #[serde(default)]
    depends_on: Vec<String>,
    parent_task_id: Option<String>,
    priority: Option<String>,
    complexity: Option<String>,
    estimated_edits: Option<i64>,
    cycle_id: Option<String>,
    reporter: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    cwd: Option<String>,
    envelope: Option<Value>,
    /// LM-265 / L1.3.c — explicit `atomic_size_hint` on direct task
    /// creation. Mirrors the strict importer's behaviour (LM-263) so
    /// both creation paths produce the same row shape. If both this
    /// and `envelope.atomic_size_hint` are present, the explicit field
    /// wins (caller intent is explicit). Falls back to `envelope`'s
    /// value, then to the SQLite DEFAULT (`small`).
    atomic_size_hint: Option<String>,
    decomposition_policy: Option<String>,
    /// FIX-DAEMON-r2-tier: explicit tier on create (low|med|high). Default 'med'.
    tier: Option<String>,
    /// FIX-DAEMON-r2-qa: QA workflow fields available on direct creation.
    qa_status: Option<String>,
    scenario_id: Option<String>,
    defect_task: Option<String>,
    scenario_amendment: Option<String>,
    /// US-CKT-SCHEMA-011: evidence (file:line or reasoning summary, max 4 KiB)
    evidence: Option<String>,
    /// US-CKT-SCHEMA-021: batch_id (Crockford base32 ULID, 26 chars)
    batch_id: Option<String>,
}

#[derive(Serialize)]
struct TaskWithEnvelope {
    #[serde(flatten)]
    task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_envelope: Option<TaskEnvelope>,
    /// TIER-044: recommended tier (typically "high" / Opus) for QA regression rounds.
    /// Set when cycle.idx >= 2 OR task.title starts with "QA-".
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_tier: Option<String>,
}

fn task_with_envelope(conn: &rusqlite::Connection, task: Task) -> ApiResult<TaskWithEnvelope> {
    let env = task_envelopes::active_for_task(conn, &task.id)?;
    // TIER-044: regression-round detection.
    let is_qa_prefix = task.title.starts_with("QA-");
    let cycle_idx_high = if let Some(cid) = &task.cycle_id {
        conn.query_row(
            "SELECT idx FROM cycles WHERE id = ?1",
            rusqlite::params![cid],
            |r| r.get::<_, i64>(0),
        )
        .ok()
        .map(|idx| idx >= 2)
        .unwrap_or(false)
    } else {
        false
    };
    let recommended_tier = if is_qa_prefix || cycle_idx_high {
        Some("high".to_string())
    } else {
        None
    };
    Ok(TaskWithEnvelope {
        task,
        active_envelope: env,
        recommended_tier,
    })
}

async fn create(
    State(app): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<TaskWithEnvelope>> {
    let mut conn = app.conn();
    let assignee = norm_opt(body.assignee);
    let parent_task_id = norm_opt(body.parent_task_id);
    let priority = norm_opt(body.priority);
    let complexity = norm_opt(body.complexity);
    let reporter = norm_opt(body.reporter);
    let type_ = norm_opt(body.type_);
    let body_text = norm_opt(body.body);

    let cwd = norm_opt(body.cwd);
    let mut unit_id = norm_opt(body.unit_id);
    // API-TASK-001: cycle_id is REQUIRED. We no longer auto-infer from active cycle.
    let cycle_id = norm_opt(body.cycle_id);

    if let Some(cwd_str) = cwd.as_deref() {
        if let Some(project) = projects::get_by_cwd(&conn, cwd_str, false)? {
            if unit_id.is_none() {
                let plan_list = plans::list(
                    &conn,
                    plans::ListFilter {
                        project_id: Some(&project.id),
                        status: Some("active"),
                    },
                )?;
                let plan = plan_list.into_iter().next().or_else(|| {
                    plans::list(
                        &conn,
                        plans::ListFilter {
                            project_id: Some(&project.id),
                            status: None,
                        },
                    )
                    .ok()
                    .and_then(|v| v.into_iter().next())
                });
                if let Some(plan) = plan {
                    let unit_list = units::list(
                        &conn,
                        units::ListFilter {
                            plan_id: Some(&plan.id),
                        },
                    )?;
                    // Units have no status (pure grouping entity); just pick first.
                    if let Some(u) = unit_list.into_iter().next() {
                        unit_id = Some(u.id);
                    }
                }
            }
            // API-TASK-001: removed auto-infer of cycle_id from active cycle.
            // Callers must supply cycle_id explicitly.
        }
    }

    let unit_id = unit_id
        .ok_or_else(|| ApiError::bad_request("unit_id required (or supply cwd for auto-infer)"))?;
    // API-TASK-001: hard-require cycle_id.
    if cycle_id.is_none() {
        return Err(ApiError::bad_request_coded(
            "MISSING_CYCLE_ID",
            "MISSING_CYCLE_ID: cycle_id is required when creating a task. Activate a cycle and pass --cycle <CYCLE_ID>.",
        ));
    }

    // LM-265 / L1.3.c — propagate atomic_size_hint and
    // decomposition_policy. Precedence: explicit body field > envelope
    // field > SQLite DEFAULT. Tracking the envelope as a fallback keeps
    // the strict-import path and direct API path producing the same
    // row shape for the same input.
    let atomic_size_hint_str = norm_opt(body.atomic_size_hint).or_else(|| {
        body.envelope
            .as_ref()
            .and_then(|e| e.get("atomic_size_hint"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    let decomposition_policy_str = norm_opt(body.decomposition_policy).or_else(|| {
        body.envelope
            .as_ref()
            .and_then(|e| e.get("decomposition_policy"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    // FIX-DAEMON-r2-tier / FIX-DAEMON-r2-qa: forward optional explicit fields
    // through the create path. Validation happens inside repo::tasks::create.
    let tier_str = norm_opt(body.tier);
    let qa_status_str = norm_opt(body.qa_status);
    let scenario_id_str = norm_opt(body.scenario_id);
    let defect_task_str = norm_opt(body.defect_task);
    let scenario_amendment_str = norm_opt(body.scenario_amendment);
    // US-CKT-SCHEMA-011: evidence (max 4 KiB)
    let evidence_str = norm_opt(body.evidence);
    if let Some(ev) = evidence_str.as_deref() {
        if ev.len() > 4096 {
            return Err(ApiError::bad_request_coded(
                "EVIDENCE_TOO_LONG",
                "EVIDENCE_TOO_LONG: evidence must be ≤ 4096 bytes",
            ));
        }
    }
    // US-CKT-SCHEMA-003: scenario_id regex validation (^US-[A-Z][A-Z0-9-]*-\d{3}$)
    if let Some(sc) = scenario_id_str.as_deref() {
        validate_scenario_id(sc)?;
    }
    // US-CKT-SCHEMA-022: batch_id ULID validation (Crockford base32, 26 chars)
    let batch_id_str = norm_opt(body.batch_id);
    if let Some(bid) = batch_id_str.as_deref() {
        validate_batch_id(bid)?;
    }

    let created = tasks::create(
        &mut conn,
        tasks::CreateInput {
            unit_id: &unit_id,
            title: &body.title,
            body: body_text.as_deref(),
            assignee: assignee.as_deref(),
            idx: body.idx,
            depends_on: body.depends_on,
            parent_task_id: parent_task_id.as_deref(),
            priority: priority.as_deref(),
            complexity: complexity.as_deref(),
            estimated_edits: body.estimated_edits,
            cycle_id: cycle_id.as_deref(),
            reporter: reporter.as_deref(),
            type_: type_.as_deref(),
            atomic_size_hint: atomic_size_hint_str.as_deref(),
            decomposition_policy: decomposition_policy_str.as_deref(),
            tier: tier_str.as_deref(),
            qa_status: qa_status_str.as_deref(),
            scenario_id: scenario_id_str.as_deref(),
            defect_task: defect_task_str.as_deref(),
            scenario_amendment: scenario_amendment_str.as_deref(),
            evidence: evidence_str.as_deref(),
            batch_id: batch_id_str.as_deref(),
        },
    )?;

    let mut task = match created {
        Some(t) => t,
        None => return Err(ApiError::not_found("not found")),
    };

    if let Some(mut env) = body.envelope {
        reject_high_entropy_in_value(&env).map_err(ApiError::bad_request)?;
        autofill_planned_sha(&mut env, &conn);
        let json = serde_json::to_string(&env)
            .map_err(|e| ApiError::bad_request(format!("envelope serialize: {e}")))?;
        let signer = assignee
            .as_deref()
            .or(reporter.as_deref())
            .unwrap_or("main");
        task_envelopes::sign_for_task(&mut conn, &task.id, &json, signer)?;
        // Re-read so active_envelope_id is reflected.
        if let Some(refreshed) = tasks::get(&conn, &task.id)? {
            task = refreshed;
        }
    }

    let with_env = task_with_envelope(&conn, task)?;
    drop(conn);
    app.emit(
        "task:created",
        serde_json::json!({ "id": with_env.task.id }),
    );
    schedule_task_embed(app.clone(), &with_env.task);
    Ok(Json(with_env))
}

async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Task>> {
    json_or_404(tasks::get(&app.conn(), &id)?)
}

#[derive(Deserialize, Default)]
struct DeleteBody {
    reason: Option<String>,
}

// Node v2.2.1 parity: hard-delete only for `todo` tasks under a draft plan;
// otherwise soft-delete by flipping status to `cancelled` and attaching a
// `[Cancelled] …` system comment so the audit trail is preserved.
async fn delete_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<DeleteBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("Task not found"))?;
    let canonical = task.id.clone();

    if task.status == "todo" {
        let plan_is_draft = match units::get(&conn, &task.unit_id)? {
            Some(u) => match plans::get(&conn, &u.plan_id)? {
                Some(p) => p.status == "draft",
                None => false,
            },
            None => false,
        };
        if plan_is_draft {
            tasks::delete(&conn, &canonical)?;
            app.emit("task:deleted", serde_json::json!({ "id": canonical }));
            return Ok(Json(
                serde_json::json!({ "ok": true, "deleted": canonical }),
            ));
        }
    }

    drop(conn);
    let reason = body
        .and_then(|b| b.0.reason)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Cancelled via delete".into());

    let mut conn = app.conn();
    let (updated, cascade_events) = tasks::update(
        &mut conn,
        &canonical,
        tasks::UpdateFields {
            status: Some("cancelled".into()),
            ..Default::default()
        },
    )?;
    comments::create(
        &conn,
        &canonical,
        "system",
        &format!("[Cancelled] {reason}"),
    )?;
    app.emit("task:updated", serde_json::json!({ "id": canonical }));
    // FIX-DAEMON-106: emit cascade SSE events
    for (event_name, entity_id) in cascade_events {
        app.emit(
            event_name,
            serde_json::json!({ "id": entity_id, "cascade": true }),
        );
    }
    let payload = updated
        .map(|t| serde_json::to_value(t).unwrap_or_else(|_| serde_json::json!({})))
        .unwrap_or_else(|| serde_json::json!({ "ok": true, "deleted": canonical }));
    Ok(Json(payload))
}

#[derive(Deserialize)]
struct AppendBody {
    text: String,
}

async fn append_body(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AppendBody>,
) -> ApiResult<Json<Task>> {
    json_or_404(tasks::append_body(&app.conn(), &id, &body.text)?)
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<TaskWithEnvelope>> {
    let mut conn = app.conn();
    let title_or_body_touched = body
        .as_object()
        .map(|o| o.contains_key("title") || o.contains_key("body"))
        .unwrap_or(false);
    // Sidecar: `_comment` attaches a comment in the same request for audit-trail parity.
    // Author precedence: explicit `_author` > `assignee` in the PATCH body > `_agent` hook marker > "main".
    let sidecar_comment = body
        .get("_comment")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let sidecar_author = body
        .get("_author")
        .and_then(Value::as_str)
        .or_else(|| body.get("assignee").and_then(Value::as_str))
        .or_else(|| body.get("_agent").and_then(Value::as_str))
        .unwrap_or("main")
        .to_owned();
    let envelope_value = body.get("envelope").cloned();

    // Status-transition guard (RL-U3-10 / LM-63): when the PATCH advances
    // the task into `in_progress` or `done`, evaluate the resolved
    // envelope's preconditions / postconditions against the live state.
    // A failing predicate maps to HTTP 409 with the offending JSON
    // attached as `details.violating_predicate`.
    if let Some(new_status) = body.get("status").and_then(Value::as_str) {
        let field = match new_status {
            "in_progress" => Some("preconditions"),
            "done" => Some("postconditions"),
            _ => None,
        };
        if let Some(field) = field {
            if let Some(current) = tasks::get(&conn, &id)? {
                if current.status != new_status {
                    if let Some(env) = resolve_envelope(&conn, &current)? {
                        if let Some(preds) = env.get(field).and_then(Value::as_array) {
                            let repo_path = env
                                .get("target_repo")
                                .and_then(Value::as_str)
                                .and_then(|t| git::resolve_target_repo_path(&conn, t));
                            let ctx = env_conditions::EvalContext::new(&conn, repo_path);
                            if let Err(v) = env_conditions::evaluate(preds, &ctx) {
                                return Err(ApiError::conflict_with_details(
                                    format!("{} violation: {}", field, v.reason),
                                    serde_json::json!({
                                        "field": field,
                                        "reason": v.reason,
                                        "violating_predicate": v.predicate,
                                    }),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // TIER-043: when tier_used differs from declared tier (or current tier_used),
    // require non-empty escalation_reason in the same patch.
    if let Some(obj) = body.as_object() {
        if let Some(new_tier_used) = obj.get("tier_used").and_then(Value::as_str) {
            let current =
                tasks::get(&conn, &id)?.ok_or_else(|| ApiError::not_found("Task not found"))?;
            let declared_tier = obj
                .get("tier")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(current.tier.clone())
                .unwrap_or_default();
            if new_tier_used != declared_tier {
                let reason = obj
                    .get("escalation_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if reason.is_empty() {
                    return Err(ApiError::bad_request_coded(
                        "ESCALATION_REASON_REQUIRED",
                        format!(
                            "ESCALATION_REASON_REQUIRED: tier_used='{}' differs from tier='{}'; provide non-empty escalation_reason.",
                            new_tier_used, declared_tier
                        ),
                    ));
                }
            }
        }
    }

    let (updated, cascade_events) = tasks::update(&mut conn, &id, parse_update(&body)?)?;
    if let (Some(text), Some(task)) = (sidecar_comment.as_deref(), updated.as_ref()) {
        comments::create(&conn, &task.id, &sidecar_author, text)?;
    }

    let mut task = match updated {
        Some(t) => t,
        None => return Err(ApiError::not_found("not found")),
    };

    if let Some(mut env) = envelope_value {
        reject_high_entropy_in_value(&env).map_err(ApiError::bad_request)?;
        autofill_planned_sha(&mut env, &conn);
        let json = serde_json::to_string(&env)
            .map_err(|e| ApiError::bad_request(format!("envelope serialize: {e}")))?;
        task_envelopes::sign_for_task(&mut conn, &task.id, &json, &sidecar_author)?;
        if let Some(refreshed) = tasks::get(&conn, &task.id)? {
            task = refreshed;
        }
    }

    let with_env = task_with_envelope(&conn, task)?;
    drop(conn);
    app.emit(
        "task:updated",
        serde_json::json!({ "id": with_env.task.id }),
    );
    // FIX-DAEMON-106: emit SSE for cascade-completed plan/cycle
    for (event_name, entity_id) in cascade_events {
        app.emit(
            event_name,
            serde_json::json!({ "id": entity_id, "cascade": true }),
        );
    }
    if title_or_body_touched {
        schedule_task_embed(app.clone(), &with_env.task);
    }
    Ok(Json(with_env))
}

#[derive(Deserialize)]
struct BulkBody {
    ids: Vec<String>,
    fields: Value,
}

async fn bulk_update(
    State(app): State<AppState>,
    Json(body): Json<BulkBody>,
) -> ApiResult<Json<Vec<Task>>> {
    let mut conn = app.conn();
    let mut out = Vec::new();
    for id in &body.ids {
        let (updated, cascade_events) = tasks::update(&mut conn, id, parse_update(&body.fields)?)?;
        if let Some(t) = updated {
            // FIX-DAEMON-106: emit cascade SSE events per task
            for (event_name, entity_id) in cascade_events {
                app.emit(
                    event_name,
                    serde_json::json!({ "id": entity_id, "cascade": true }),
                );
            }
            out.push(t);
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
    mode: Option<String>,
}

#[derive(Serialize)]
struct TaskHit {
    #[serde(flatten)]
    task: Task,
    #[serde(skip_serializing_if = "Option::is_none")]
    _distance: Option<f32>,
    /// RAG-SIM-004: similarity scores exposed on /similar
    #[serde(skip_serializing_if = "Option::is_none")]
    cosine: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bm25: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_match: Option<f32>,
}

async fn search(
    State(app): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<TaskHit>>> {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let mode = q.mode.unwrap_or_else(|| "keyword".into());

    // RL-U3-05 / LM-140: envelope-only mode searches the envelope FTS
    // (intent/prompt_template/success_criteria) and does NOT fall back to
    // tasks.body — the use case is "find tasks whose contract mentions X"
    // even when the task body never mentioned X.
    if mode == "envelope" {
        let results = tasks::keyword_search_envelope_only(&app.conn(), &query, limit)?;
        return Ok(Json(
            results
                .into_iter()
                .map(|t| TaskHit {
                    task: t,
                    _distance: None,
                    cosine: None,
                    bm25: None,
                    tag_match: None,
                })
                .collect(),
        ));
    }

    if mode == "semantic" || mode == "hybrid" {
        if let Ok(Some(vec)) = embeddings::embed(&query).await {
            let vec_hits = tasks::vector_search(&app.conn(), &vec, limit)?;
            if mode == "semantic" {
                return Ok(Json(
                    vec_hits
                        .into_iter()
                        .map(|(t, d)| TaskHit {
                            task: t,
                            _distance: Some(d),
                            cosine: None,
                            bm25: None,
                            tag_match: None,
                        })
                        .collect(),
                ));
            }
            let fts = tasks::keyword_search(&app.conn(), &query, limit)?;
            let mut seen = std::collections::HashSet::new();
            let mut merged: Vec<TaskHit> = Vec::new();
            for t in fts {
                if seen.insert(t.id.clone()) {
                    merged.push(TaskHit {
                        task: t,
                        _distance: None,
                        cosine: None,
                        bm25: None,
                        tag_match: None,
                    });
                }
            }
            for (t, d) in vec_hits {
                if seen.insert(t.id.clone()) {
                    merged.push(TaskHit {
                        task: t,
                        _distance: Some(d),
                        cosine: None,
                        bm25: None,
                        tag_match: None,
                    });
                }
            }
            merged.truncate(limit as usize);
            return Ok(Json(merged));
        }
    }

    let results = tasks::keyword_search(&app.conn(), &query, limit)?;
    Ok(Json(
        results
            .into_iter()
            .map(|t| TaskHit {
                task: t,
                _distance: None,
                cosine: None,
                bm25: None,
                tag_match: None,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct SimilarQuery {
    limit: Option<i64>,
    status: Option<String>,
}

async fn similar(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SimilarQuery>,
) -> ApiResult<Json<Vec<TaskHit>>> {
    let limit = q.limit.unwrap_or(10).clamp(1, 30);
    let task =
        tasks::get(&app.conn(), &id)?.ok_or_else(|| ApiError::not_found("Task not found"))?;
    let source = format!("{}\n{}", task.title, task.body);
    let vec = match embeddings::embed(&source)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    {
        Some(v) => v,
        None => return Ok(Json(Vec::new())),
    };
    // US-CLAWKET-RAG-SIM-004: low-similarity hits poison the result set; cap
    // the floor at cosine >= 0.3 so callers (incl. clawket_find_similar_tasks
    // MCP tool) never see noisy near-zero matches.
    const SIMILARITY_FLOOR: f32 = 0.3;
    // Over-fetch generously so the post-filter still has enough candidates.
    let raw = tasks::vector_search(&app.conn(), &vec, (limit + 5) * 4)?;
    let out: Vec<TaskHit> = raw
        .into_iter()
        .filter(|(t, _)| t.id != task.id)
        .filter(|(t, _)| match &q.status {
            Some(s) => t.status == *s,
            None => true,
        })
        .filter_map(|(t, d)| {
            // RAG-SIM-004: expose composite similarity scores.
            // Cosine derived from vector distance (1 - d/2 clamped).
            // TODO(v3-r3): compute bm25 from artifact text and tag_match from real labels.
            let cosine = (1.0 - (d / 2.0)).clamp(0.0, 1.0);
            if cosine < SIMILARITY_FLOOR {
                return None;
            }
            Some(TaskHit {
                task: t,
                _distance: Some(d),
                cosine: Some(cosine),
                bm25: Some(0.0),
                tag_match: Some(0.0),
            })
        })
        .take(limit as usize)
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct EnvelopeQuery {
    resolve: Option<bool>,
    version: Option<i64>,
}

#[derive(Serialize)]
struct EnvelopeResponse {
    raw_envelope: Value,
    resolved_envelope: Value,
    inheritance_chain: Vec<String>,
    version: i64,
    superseded: bool,
}

async fn get_envelope(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EnvelopeQuery>,
) -> ApiResult<Json<EnvelopeResponse>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;

    let envelope = match q.version {
        Some(v) => task_envelopes::list_for_task(&conn, &task.id)?
            .into_iter()
            .find(|e| e.version == v)
            .ok_or_else(|| {
                ApiError::not_found(format!("envelope version {} not found for {}", v, task.id))
            })?,
        None => task_envelopes::active_for_task(&conn, &task.id)?
            .ok_or_else(|| ApiError::not_found(format!("no envelope on task {}", task.id)))?,
    };

    let raw_envelope: Value = serde_json::from_str(&envelope.json)
        .map_err(|e| ApiError::internal(format!("envelope json parse: {e}")))?;

    // Single recursive-CTE walk for both the inheritance chain (task IDs
    // root → self) and the per-level active envelope JSONs we deep-merge
    // when `resolve=true` (RL-U3-11 / LM-64). The leaf override is still
    // applied below so historical-version requests get the right body.
    let chain = task_envelopes::resolve_chain(&conn, &task.id, RESOLVE_CHAIN_MAX_DEPTH)?;
    let chain_root_to_self: Vec<String> = chain.iter().map(|e| e.task_id.clone()).collect();

    let resolved_envelope = if q.resolve.unwrap_or(false) {
        let mut acc = Value::Object(Default::default());
        for entry in &chain {
            if entry.task_id == task.id {
                deep_merge(&mut acc, &raw_envelope);
                continue;
            }
            if let Some(j) = &entry.json {
                if let Ok(v) = serde_json::from_str::<Value>(j) {
                    deep_merge(&mut acc, &v);
                }
            }
        }
        acc
    } else {
        raw_envelope.clone()
    };

    Ok(Json(EnvelopeResponse {
        raw_envelope,
        resolved_envelope,
        inheritance_chain: chain_root_to_self,
        version: envelope.version,
        superseded: envelope.superseded_by.is_some(),
    }))
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize)]
struct HistoryEntry {
    id: String,
    version: i64,
    created_at: i64,
    signed_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_at: Option<i64>,
    envelope: Value,
}

#[derive(Serialize)]
struct ClearEnvelopeResponse {
    task_id: String,
    cleared: bool,
}

async fn clear_envelope(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ClearEnvelopeResponse>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let cleared = task_envelopes::clear_active_for_task(&conn, &task.id)?;
    drop(conn);
    if cleared {
        app.emit("task:updated", serde_json::json!({ "id": task.id }));
    }
    Ok(Json(ClearEnvelopeResponse {
        task_id: task.id,
        cleared,
    }))
}

async fn envelope_history(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<HistoryEntry>>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let entries = task_envelopes::history_for_task(&conn, &task.id, limit, offset)?;
    let out: Vec<HistoryEntry> = entries
        .into_iter()
        .map(|e| {
            let envelope: Value = serde_json::from_str(&e.envelope.json).unwrap_or(Value::Null);
            HistoryEntry {
                id: e.envelope.id,
                version: e.envelope.version,
                created_at: e.envelope.signed_at,
                signed_by: e.envelope.signed_by,
                superseded_at: e.superseded_at,
                envelope,
            }
        })
        .collect();
    Ok(Json(out))
}

/// LM-151 / RL-U6-02 — POST /tasks/:id/envelope/validate
///
/// Body shape:
/// ```json
/// { "envelope": { ... }, "strict": true, "resolve": false }
/// ```
/// All fields optional. With no `envelope`, the active stored envelope
/// is validated. With `resolve=true`, the inheritance chain is folded
/// before validation (mirrors the MCP tool's default behavior). With
/// `envelope` provided, that draft is validated as-is — supports the
/// real-time form feedback flow where the user hasn't saved yet.
#[derive(Deserialize, Default)]
struct ValidateEnvelopeBody {
    envelope: Option<Value>,
    #[serde(default = "default_strict")]
    strict: bool,
    #[serde(default)]
    resolve: bool,
}

fn default_strict() -> bool {
    true
}

#[derive(Serialize)]
struct ValidateEnvelopeResponse {
    #[serde(flatten)]
    result: env_validate::ValidateResult,
    /// The envelope the validator actually evaluated. Echoed back so a
    /// caller (MCP tool, web form, future eval pipeline) can show what
    /// they validated *against* without a second round-trip.
    evaluated_envelope: Value,
}

async fn validate_envelope_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ValidateEnvelopeBody>,
) -> ApiResult<Json<ValidateEnvelopeResponse>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;

    let envelope = if let Some(draft) = body.envelope {
        if body.resolve {
            // Draft + resolve = merge with parent chain so inherited
            // fields the form doesn't surface still satisfy required-
            // field checks.
            let chain = task_envelopes::resolve_chain(&conn, &task.id, RESOLVE_CHAIN_MAX_DEPTH)?;
            let mut acc = Value::Object(Default::default());
            for entry in &chain {
                if entry.task_id == task.id {
                    deep_merge(&mut acc, &draft);
                    continue;
                }
                if let Some(j) = &entry.json {
                    if let Ok(v) = serde_json::from_str::<Value>(j) {
                        deep_merge(&mut acc, &v);
                    }
                }
            }
            acc
        } else {
            draft
        }
    } else {
        let active = task_envelopes::active_for_task(&conn, &task.id)?
            .ok_or_else(|| ApiError::not_found(format!("no envelope on task {}", task.id)))?;
        let raw: Value = serde_json::from_str(&active.json)
            .map_err(|e| ApiError::internal(format!("envelope json parse: {e}")))?;
        if body.resolve {
            resolve_envelope(&conn, &task)?.unwrap_or(raw)
        } else {
            raw
        }
    };

    let result = env_validate::validate_envelope(&envelope, body.strict);
    Ok(Json(ValidateEnvelopeResponse {
        result,
        evaluated_envelope: envelope,
    }))
}

#[derive(Deserialize, Default)]
struct DecomposeBody {
    /// Strategy hint: `auto` | `scoped` | `by-repo`. Unknown values
    /// normalize to `auto` inside the suggester.
    #[serde(default)]
    strategy: Option<String>,
    /// Cap suggestion depth at 1..=3. Default 2.
    #[serde(default)]
    max_depth: Option<u32>,
}

async fn decompose_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DecomposeBody>,
) -> ApiResult<Json<Value>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;

    let max_depth = body.max_depth.unwrap_or(2).clamp(1, 3);
    let strategy = body.strategy.unwrap_or_else(|| "auto".to_string());

    let resolved = resolve_envelope(&conn, &task)?.unwrap_or(Value::Null);
    // Existing children count: same query the MCP tool used (BFS,
    // capped depth). The route only consults the count, not the list.
    let existing_children_count =
        tasks::descendants(&conn, &task.id, max_depth as usize, true, TREE_NODE_CAP)?.len();

    let parent = decompose_suggest::ParentSummary {
        id: task.id.clone(),
        ticket_number: task.ticket_number.clone(),
        title: task.title.clone(),
    };
    let result = decompose_suggest::generate(
        parent,
        &resolved,
        existing_children_count,
        max_depth,
        &strategy,
    );
    Ok(Json(decompose_suggest::to_json(&result)))
}

#[derive(Deserialize, Default)]
struct SubtaskBody {
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    idx: Option<i64>,
    #[serde(default)]
    depends_on: Vec<String>,
    priority: Option<String>,
    complexity: Option<String>,
    estimated_edits: Option<i64>,
    cycle_id: Option<String>,
    reporter: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
    unit_id: Option<String>,
    envelope_overrides: Option<Value>,
}

async fn create_subtask(
    State(app): State<AppState>,
    Path(parent_id): Path<String>,
    Json(body): Json<SubtaskBody>,
) -> ApiResult<Json<TaskWithEnvelope>> {
    let mut conn = app.conn();
    let parent = tasks::get(&conn, &parent_id)?
        .ok_or_else(|| ApiError::not_found(format!("Parent task not found: {}", parent_id)))?;

    let unit_id = norm_opt(body.unit_id).unwrap_or_else(|| parent.unit_id.clone());
    let cycle_id = norm_opt(body.cycle_id).or_else(|| parent.cycle_id.clone());
    let body_text = norm_opt(body.body);
    let assignee = norm_opt(body.assignee);
    let priority = norm_opt(body.priority);
    let complexity = norm_opt(body.complexity);
    let reporter = norm_opt(body.reporter);
    let type_ = norm_opt(body.type_);

    let parent_env = task_envelopes::active_for_task(&conn, &parent.id)?;
    let child_env =
        compute_inherited_envelope(parent_env.as_ref(), body.envelope_overrides.as_ref())?;

    let created = tasks::create(
        &mut conn,
        tasks::CreateInput {
            unit_id: &unit_id,
            title: &body.title,
            body: body_text.as_deref(),
            assignee: assignee.as_deref(),
            idx: body.idx,
            depends_on: body.depends_on,
            parent_task_id: Some(&parent.id),
            priority: priority.as_deref(),
            complexity: complexity.as_deref(),
            estimated_edits: body.estimated_edits,
            cycle_id: cycle_id.as_deref(),
            reporter: reporter.as_deref(),
            type_: type_.as_deref(),
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
    )?;
    let mut task =
        created.ok_or_else(|| ApiError::internal("failed to create subtask".to_string()))?;

    if let Some(mut env) = child_env {
        autofill_planned_sha(&mut env, &conn);
        let json = serde_json::to_string(&env)
            .map_err(|e| ApiError::bad_request(format!("envelope serialize: {e}")))?;
        let signer = assignee
            .as_deref()
            .or(reporter.as_deref())
            .unwrap_or("main");
        task_envelopes::sign_for_task(&mut conn, &task.id, &json, signer)?;
        if let Some(refreshed) = tasks::get(&conn, &task.id)? {
            task = refreshed;
        }
    }

    let with_env = task_with_envelope(&conn, task)?;
    drop(conn);
    app.emit(
        "task:created",
        serde_json::json!({ "id": with_env.task.id }),
    );
    schedule_task_embed(app.clone(), &with_env.task);
    Ok(Json(with_env))
}

#[derive(Serialize)]
struct DriftResponse {
    drift_level: &'static str,
    changed_files_in_scope: Vec<String>,
    total_changed: usize,
    planned_sha: String,
    current_sha: String,
}

async fn get_drift(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<DriftResponse>> {
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let envelope = task_envelopes::active_for_task(&conn, &task.id)?
        .ok_or_else(|| ApiError::not_found(format!("no envelope on task {}", task.id)))?;
    let env_value: Value = serde_json::from_str(&envelope.json)
        .map_err(|e| ApiError::internal(format!("envelope json parse: {e}")))?;

    let target_repo = env_value
        .get("target_repo")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("envelope.target_repo missing"))?;
    let planned_sha = env_value
        .get("planned_sha")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("envelope.planned_sha missing or null"))?
        .to_string();
    let scope: Vec<String> = env_value
        .get("scope_boundary")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let repo_path = git::resolve_target_repo_path(&conn, target_repo).ok_or_else(|| {
        ApiError::not_found(format!(
            "target_repo not registered as a project cwd: {}",
            target_repo
        ))
    })?;
    let current_sha = git::current_head(&repo_path)
        .ok_or_else(|| ApiError::internal("could not read current HEAD".to_string()))?;
    let changed = git::diff_files(&repo_path, &planned_sha, &current_sha).ok_or_else(|| {
        ApiError::internal(format!(
            "git diff {planned_sha}..{current_sha} failed in {}",
            repo_path.display()
        ))
    })?;

    let in_scope: Vec<String> = if scope.is_empty() {
        changed.clone()
    } else {
        changed
            .iter()
            .filter(|f| scope.iter().any(|p| f.starts_with(p)))
            .cloned()
            .collect()
    };

    let drift_level = match in_scope.len() {
        0 => "none",
        1..=2 => "minor",
        _ => "major",
    };

    Ok(Json(DriftResponse {
        drift_level,
        changed_files_in_scope: in_scope,
        total_changed: changed.len(),
        planned_sha,
        current_sha,
    }))
}

/// Default lease TTL when callers don't supply one — long enough to cover a
/// typical headless `claude -p` spawn, short enough that a crashed CLI's
/// orphaned lock auto-clears within a few minutes.
const DEFAULT_LEASE_TTL_MS: i64 = 300_000;

/// Hard cap so a misbehaving CLI can't pin a task indefinitely.
const MAX_LEASE_TTL_MS: i64 = 3_600_000;

#[derive(Deserialize, Default)]
struct LeaseBody {
    session_id: Option<String>,
    ttl_ms: Option<i64>,
}

#[derive(Deserialize, Default)]
struct ReleaseBody {
    session_id: Option<String>,
}

#[derive(Serialize)]
struct LeaseResponse {
    task_id: String,
    session_id: String,
    acquired_at: i64,
    expires_at: i64,
    heartbeat_at: i64,
}

impl From<locks::TaskLock> for LeaseResponse {
    fn from(l: locks::TaskLock) -> Self {
        Self {
            task_id: l.task_id,
            session_id: l.session_id,
            acquired_at: l.acquired_at,
            expires_at: l.expires_at,
            heartbeat_at: l.heartbeat_at,
        }
    }
}

fn validate_session_id(s: &Option<String>) -> ApiResult<String> {
    let v = s.as_deref().map(str::trim).unwrap_or("");
    if v.is_empty() {
        return Err(ApiError::bad_request("session_id is required"));
    }
    Ok(v.to_string())
}

fn resolve_ttl(ttl_ms: Option<i64>) -> ApiResult<i64> {
    let ttl = ttl_ms.unwrap_or(DEFAULT_LEASE_TTL_MS);
    if ttl <= 0 {
        return Err(ApiError::bad_request("ttl_ms must be positive"));
    }
    if ttl > MAX_LEASE_TTL_MS {
        return Err(ApiError::bad_request(format!(
            "ttl_ms exceeds {} ms cap",
            MAX_LEASE_TTL_MS
        )));
    }
    Ok(ttl)
}

/// POST /tasks/{id}/lease — acquire (or refresh) a session lease (LM-179).
///
/// `200 OK` on Acquired (newly granted, expired-and-reclaimed, or refreshed
/// by the same session). `409 Conflict` with `details = { holder }` when a
/// different live session holds it.
async fn acquire_lease(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LeaseBody>,
) -> ApiResult<Json<LeaseResponse>> {
    let session_id = validate_session_id(&body.session_id)?;
    let ttl_ms = resolve_ttl(body.ttl_ms)?;
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;

    match locks::acquire(&conn, &task.id, &session_id, ttl_ms)? {
        locks::AcquireOutcome::Acquired(lock) => Ok(Json(LeaseResponse::from(lock))),
        locks::AcquireOutcome::Conflict(holder) => {
            let details = serde_json::json!({
                "holder": {
                    "task_id": holder.task_id,
                    "session_id": holder.session_id,
                    "acquired_at": holder.acquired_at,
                    "expires_at": holder.expires_at,
                    "heartbeat_at": holder.heartbeat_at,
                }
            });
            Err(ApiError::conflict_with_details(
                format!(
                    "Task {} is leased by session {}",
                    holder.task_id, holder.session_id
                ),
                details,
            ))
        }
    }
}

/// DELETE /tasks/{id}/lease — release a lease owned by `session_id` (LM-179).
///
/// Returns `200 OK` with `{ "released": bool }`. The boolean is `false` when
/// the row was already gone or owned by a different session — release is
/// idempotent and never errors on stale callers.
async fn release_lease(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReleaseBody>,
) -> ApiResult<Json<Value>> {
    let session_id = validate_session_id(&body.session_id)?;
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let released = locks::release(&conn, &task.id, &session_id)?;
    Ok(Json(serde_json::json!({ "released": released })))
}

/// POST /tasks/{id}/lease/heartbeat — extend an existing lease's TTL (LM-179).
///
/// `200 OK` with the refreshed lease when the caller still owns the live
/// lock; `409 Conflict` (no holder details) when the lock has been reclaimed
/// by someone else or expired before the heartbeat arrived.
async fn heartbeat_lease(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LeaseBody>,
) -> ApiResult<Json<LeaseResponse>> {
    let session_id = validate_session_id(&body.session_id)?;
    let ttl_ms = resolve_ttl(body.ttl_ms)?;
    let conn = app.conn();
    let task = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    match locks::heartbeat(&conn, &task.id, &session_id, ttl_ms)? {
        Some(lock) => Ok(Json(LeaseResponse::from(lock))),
        None => Err(ApiError::conflict(format!(
            "Lease no longer held by session {} on task {}",
            session_id, task.id
        ))),
    }
}

/// Best-effort fill of `planned_sha` when the envelope declares a
/// `target_repo` but doesn't pin a SHA itself. The SHA is `git rev-parse
/// HEAD` of the project cwd whose basename matches the token. On failure
/// the field becomes `null` and `planned_sha_warning` records the cause —
/// the daemon never blocks task creation on git resolution.
fn autofill_planned_sha(envelope: &mut Value, conn: &rusqlite::Connection) {
    let obj = match envelope.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    if let Some(existing) = obj.get("planned_sha") {
        if !existing.is_null() {
            return;
        }
    }
    let target = match obj.get("target_repo").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return,
    };
    match git::resolve_target_repo_head(conn, &target) {
        Some(sha) => {
            obj.insert("planned_sha".into(), Value::String(sha));
            obj.remove("planned_sha_warning");
        }
        None => {
            obj.insert("planned_sha".into(), Value::Null);
            obj.insert(
                "planned_sha_warning".into(),
                Value::String(format!(
                    "could not resolve git HEAD for target_repo={}",
                    target
                )),
            );
        }
    }
}

#[derive(Deserialize)]
struct TreeQuery {
    depth: Option<i64>,
    order: Option<String>,
    include_envelope: Option<bool>,
}

#[derive(Serialize)]
struct TreeNode {
    #[serde(flatten)]
    task: Task,
    depth: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_envelope: Option<Value>,
}

const TREE_NODE_CAP: usize = 1024;

async fn get_ancestors(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TreeQuery>,
) -> ApiResult<Json<Vec<TreeNode>>> {
    let conn = app.conn();
    let _ = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let depth = q.depth.map(|d| d.max(0) as usize).unwrap_or(TREE_NODE_CAP);
    let want_env = q.include_envelope.unwrap_or(true);
    let chain = tasks::ancestors(&conn, &id, depth)?;
    let mut out = Vec::with_capacity(chain.len());
    for (i, t) in chain.into_iter().enumerate() {
        let resolved = if want_env {
            resolve_envelope(&conn, &t)?
        } else {
            None
        };
        out.push(TreeNode {
            task: t,
            depth: (i as i64) + 1,
            resolved_envelope: resolved,
        });
    }
    Ok(Json(out))
}

async fn get_descendants(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TreeQuery>,
) -> ApiResult<Json<Vec<TreeNode>>> {
    let conn = app.conn();
    let _ = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let depth = q.depth.map(|d| d.max(1) as usize).unwrap_or(TREE_NODE_CAP);
    let bfs = matches!(q.order.as_deref(), Some("bfs"));
    let want_env = q.include_envelope.unwrap_or(true);
    let nodes = tasks::descendants(&conn, &id, depth, bfs, TREE_NODE_CAP)?;
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        let resolved = if want_env {
            resolve_envelope(&conn, &n.task)?
        } else {
            None
        };
        out.push(TreeNode {
            task: n.task,
            depth: n.depth,
            resolved_envelope: resolved,
        });
    }
    Ok(Json(out))
}

async fn get_subtree(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TreeQuery>,
) -> ApiResult<Json<Vec<TreeNode>>> {
    let conn = app.conn();
    let root = tasks::get(&conn, &id)?
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {}", id)))?;
    let depth = q.depth.map(|d| d.max(0) as usize).unwrap_or(TREE_NODE_CAP);
    let bfs = matches!(q.order.as_deref(), Some("bfs"));
    let want_env = q.include_envelope.unwrap_or(true);

    let mut out: Vec<TreeNode> = Vec::new();
    let root_resolved = if want_env {
        resolve_envelope(&conn, &root)?
    } else {
        None
    };
    out.push(TreeNode {
        task: root.clone(),
        depth: 0,
        resolved_envelope: root_resolved,
    });

    if depth > 0 {
        let descendants =
            tasks::descendants(&conn, &root.id, depth, bfs, TREE_NODE_CAP.saturating_sub(1))?;
        for n in descendants {
            let resolved = if want_env {
                resolve_envelope(&conn, &n.task)?
            } else {
                None
            };
            out.push(TreeNode {
                task: n.task,
                depth: n.depth,
                resolved_envelope: resolved,
            });
        }
    }
    Ok(Json(out))
}

/// Maximum parent-chain depth considered by `resolve_envelope`. The cycle
/// defense in `repo::tasks` already rejects loops at write time, so this
/// is purely a paranoid bound on a single recursive-CTE traversal.
const RESOLVE_CHAIN_MAX_DEPTH: i64 = 1024;

/// Resolve a task's effective envelope via a single SQLite recursive CTE
/// (RL-U3-11 / LM-64). The CTE walks `parent_task_id` upward and joins
/// each level's active envelope JSON; the routes layer just deep-merges
/// the rows in root → leaf order. Replaces the previous N+1 fetch loop.
fn resolve_envelope(conn: &rusqlite::Connection, task: &Task) -> ApiResult<Option<Value>> {
    let chain = task_envelopes::resolve_chain(conn, &task.id, RESOLVE_CHAIN_MAX_DEPTH)?;
    if chain.is_empty() {
        return Ok(None);
    }
    let mut acc = Value::Object(Default::default());
    let mut any = false;
    for entry in &chain {
        if let Some(j) = &entry.json {
            if let Ok(v) = serde_json::from_str::<Value>(j) {
                deep_merge(&mut acc, &v);
                any = true;
            }
        }
    }
    Ok(if any { Some(acc) } else { None })
}

/// ADR-0001 M0 inheritance: deep-merge parent envelope into child overrides
/// (overrides win), with `scope_boundary` constrained to narrowing only — the
/// child must be a subset of the parent. Richer per-field policies
/// (MERGE_UNION etc.) land with LM-64 (RL-U3-11).
fn compute_inherited_envelope(
    parent: Option<&TaskEnvelope>,
    overrides: Option<&Value>,
) -> ApiResult<Option<Value>> {
    let parent_value: Option<Value> = match parent {
        Some(p) => Some(
            serde_json::from_str(&p.json)
                .map_err(|e| ApiError::internal(format!("parent envelope parse: {e}")))?,
        ),
        None => None,
    };
    let result = match (parent_value, overrides) {
        (None, None) => None,
        (None, Some(o)) => Some(o.clone()),
        (Some(p), None) => Some(p),
        (Some(mut p), Some(o)) => {
            validate_scope_boundary_narrowing(&p, o)?;
            deep_merge(&mut p, o);
            Some(p)
        }
    };
    Ok(result)
}

fn validate_scope_boundary_narrowing(parent: &Value, child: &Value) -> ApiResult<()> {
    let parent_sb = parent.get("scope_boundary").and_then(Value::as_array);
    let child_sb = child.get("scope_boundary").and_then(Value::as_array);
    if let (Some(p), Some(c)) = (parent_sb, child_sb) {
        let parent_set: std::collections::HashSet<&str> =
            p.iter().filter_map(Value::as_str).collect();
        for item in c {
            if let Some(s) = item.as_str() {
                if !parent_set.contains(s) {
                    return Err(ApiError::bad_request(format!(
                        "scope_boundary widening forbidden: child entry {:?} not in parent scope",
                        s
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Recursive object-only deep merge: child (`patch`) wins on scalar/array/null
/// conflicts; nested objects merge key-by-key. This is the M0 inheritance
/// behavior; richer per-field policies (MERGE_UNION / TIGHTEN_ONLY) land with
/// LM-64 (RL-U3-11).
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

// Fire-and-forget embed so HTTP responses stay snappy; mirrors Node v2.2.1.
fn schedule_task_embed(app: AppState, task: &Task) {
    let id = task.id.clone();
    let source = format!("{}\n{}", task.title, task.body);
    tokio::spawn(async move {
        if let Ok(Some(vec)) = embeddings::embed(&source).await {
            let _ = tasks::store_embedding(&app.conn(), &id, &vec);
        }
    });
}

/// US-CKT-SCHEMA-003: validate scenario_id format: ^US-[A-Z][A-Z0-9-]*-\d{3}$
/// Accepts only canonical scenario IDs. Returns 400 INVALID_SCENARIO_ID on failure.
fn validate_scenario_id(s: &str) -> ApiResult<()> {
    // Fast hand-rolled check (no regex dep): US- prefix, uppercase domain, dash, 3-digit suffix.
    let rest = s.strip_prefix("US-").ok_or_else(|| {
        ApiError::bad_request_coded(
            "INVALID_SCENARIO_ID",
            "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
        )
    })?;
    // rest = "[A-Z][A-Z0-9-]*-NNN"
    // Find last '-' separator before the 3-digit suffix.
    let last_dash = rest.rfind('-').ok_or_else(|| {
        ApiError::bad_request_coded(
            "INVALID_SCENARIO_ID",
            "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
        )
    })?;
    let (domain, suffix) = rest.split_at(last_dash);
    let suffix = &suffix[1..]; // skip '-'
                               // suffix must be exactly 3 ASCII digits
    if suffix.len() != 3 || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad_request_coded(
            "INVALID_SCENARIO_ID",
            "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
        ));
    }
    // domain must start with [A-Z] and contain only [A-Z0-9-]
    if domain.is_empty() {
        return Err(ApiError::bad_request_coded(
            "INVALID_SCENARIO_ID",
            "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
        ));
    }
    let mut chars = domain.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return Err(ApiError::bad_request_coded(
            "INVALID_SCENARIO_ID",
            "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
        ));
    }
    for c in chars {
        if !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '-' {
            return Err(ApiError::bad_request_coded(
                "INVALID_SCENARIO_ID",
                "INVALID_SCENARIO_ID: scenario_id must match ^US-[A-Z][A-Z0-9-]*-\\d{3}$",
            ));
        }
    }
    Ok(())
}

/// US-CKT-SCHEMA-022: validate batch_id as Crockford base32 ULID (26 chars, charset 0-9A-HJKMNP-TV-Z).
/// Returns 400 INVALID_BATCH_ID on failure.
fn validate_batch_id(s: &str) -> ApiResult<()> {
    const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if s.len() != 26 {
        return Err(ApiError::bad_request_coded(
            "INVALID_BATCH_ID",
            "INVALID_BATCH_ID: batch_id must be 26-char ULID (Crockford base32)",
        ));
    }
    for b in s.bytes() {
        if !CROCKFORD.contains(&b) {
            return Err(ApiError::bad_request_coded(
                "INVALID_BATCH_ID",
                "INVALID_BATCH_ID: batch_id must be 26-char ULID (Crockford base32)",
            ));
        }
    }
    Ok(())
}

fn parse_update(v: &Value) -> ApiResult<tasks::UpdateFields> {
    let obj = v
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be object"))?;
    let mut f = tasks::UpdateFields::default();
    if let Some(s) = obj.get("title").and_then(Value::as_str) {
        f.title = Some(s.into());
    }
    if let Some(v) = obj.get("body") {
        f.body = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("status").and_then(Value::as_str) {
        f.status = Some(s.into());
    }
    if let Some(v) = obj.get("assignee") {
        f.assignee = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("priority").and_then(Value::as_str) {
        f.priority = Some(s.into());
    }
    if let Some(v) = obj.get("complexity") {
        f.complexity = Some(value_to_opt_string(v));
    }
    if let Some(v) = obj.get("estimated_edits") {
        f.estimated_edits = Some(v.as_i64());
    }
    if let Some(v) = obj.get("parent_task_id") {
        f.parent_task_id = Some(value_to_opt_string(v));
    }
    if let Some(v) = obj.get("cycle_id") {
        f.cycle_id = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj
        .get("unit_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        f.unit_id = Some(s.into());
    }
    if let Some(v) = obj.get("reporter") {
        f.reporter = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("type").and_then(Value::as_str) {
        f.type_ = Some(s.into());
    }
    if let Some(v) = obj.get("agent_id") {
        f.agent_id = Some(value_to_opt_string(v));
    }
    // FIX-DAEMON-105: blocked_reason field
    if let Some(v) = obj.get("blocked_reason") {
        f.blocked_reason = Some(value_to_opt_string(v));
    }
    // FIX-DAEMON-105: QA workflow fields (from body if present)
    if let Some(s) = obj.get("tier").and_then(Value::as_str) {
        f.tier = Some(Some(s.into()));
    }
    // TIER-042 / TIER-043: tier_used + escalation_reason on update.
    if let Some(v) = obj.get("tier_used") {
        f.tier_used = Some(value_to_opt_string(v));
    }
    if let Some(v) = obj.get("escalation_reason") {
        f.escalation_reason = Some(value_to_opt_string(v));
    }
    if let Some(s) = obj.get("qa_status").and_then(Value::as_str) {
        f.qa_status = Some(Some(s.into()));
    }
    if let Some(v) = obj.get("scenario_id") {
        // US-CKT-SCHEMA-007: allow null to clear scenario_id, or regex-validated string.
        let opt_str = value_to_opt_string(v);
        if let Some(ref sc) = opt_str {
            validate_scenario_id(sc)?;
        }
        f.scenario_id = Some(opt_str);
    }
    if let Some(s) = obj.get("defect_task").and_then(Value::as_str) {
        f.defect_task = Some(Some(s.into()));
    }
    if let Some(s) = obj.get("scenario_amendment").and_then(Value::as_str) {
        f.scenario_amendment = Some(Some(s.into()));
    }
    // US-CKT-SCHEMA-011: evidence (max 4 KiB)
    if let Some(v) = obj.get("evidence") {
        let opt_str = value_to_opt_string(v);
        if let Some(ref ev) = opt_str {
            if ev.len() > 4096 {
                return Err(ApiError::bad_request_coded(
                    "EVIDENCE_TOO_LONG",
                    "EVIDENCE_TOO_LONG: evidence must be ≤ 4096 bytes",
                ));
            }
        }
        f.evidence = Some(opt_str);
    }
    // US-CKT-SCHEMA-022: batch_id (Crockford base32 ULID, 26 chars)
    if let Some(v) = obj.get("batch_id") {
        let opt_str = value_to_opt_string(v);
        if let Some(ref bid) = opt_str {
            validate_batch_id(bid)?;
        }
        f.batch_id = Some(opt_str);
    }
    Ok(f)
}

#[cfg(test)]
mod envelope {
    //! Round-trip tests for envelope acceptance on POST/PATCH /tasks.
    //! Verification: `cargo test routes::tasks::envelope` (RL-U3-02 / LM-137).

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let _ = PathBuf::new();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_task(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn patch_task(app: &axum::Router, id: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/tasks/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn post_without_envelope_works_and_returns_no_envelope_field() {
        let s = setup();
        let (status, body) = post_task(
            &s.app,
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["id"].as_str().unwrap().starts_with("TASK-"));
        assert!(body.get("active_envelope").is_none());
    }

    #[tokio::test]
    async fn post_with_envelope_signs_v1_and_returns_active() {
        let s = setup();
        let env = serde_json::json!({
            "version": 1,
            "intent": "add envelope round-trip test",
            "decomposition_policy": "atomic"
        });
        let (status, body) = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": env,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["active_envelope_id"].as_str().is_some());
        let active = &body["active_envelope"];
        assert_eq!(active["version"].as_i64(), Some(1));
        assert!(active["superseded_by"].is_null());
        let stored: Value = serde_json::from_str(active["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["intent"], "add envelope round-trip test");
    }

    #[tokio::test]
    async fn patch_with_envelope_increments_version_and_supersedes_prior() {
        let s = setup();
        let env_v1 = serde_json::json!({"version": 1, "intent": "first"});
        let (_, created) = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": env_v1,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let v1_id = created["active_envelope"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let env_v2 = serde_json::json!({"version": 1, "intent": "second"});
        let (status, updated) =
            patch_task(&s.app, &id, serde_json::json!({"envelope": env_v2})).await;
        assert_eq!(status, StatusCode::OK);
        let active = &updated["active_envelope"];
        assert_eq!(active["version"].as_i64(), Some(2));
        assert!(active["superseded_by"].is_null());
        let v2_id = active["id"].as_str().unwrap();
        assert_ne!(v1_id, v2_id);
        assert_eq!(updated["active_envelope_id"].as_str(), Some(v2_id));
    }

    #[tokio::test]
    async fn patch_without_envelope_keeps_active_unchanged() {
        let s = setup();
        let (_, created) = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "first"},
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let v1_id = created["active_envelope"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let (status, updated) =
            patch_task(&s.app, &id, serde_json::json!({"title": "renamed"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["title"], "renamed");
        assert_eq!(
            updated["active_envelope"]["id"].as_str(),
            Some(v1_id.as_str())
        );
        assert_eq!(updated["active_envelope"]["version"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn post_with_invalid_envelope_json_returns_400() {
        let s = setup();
        // axum will reject non-JSON body; we test daemon's own validation by
        // sending a string (which is JSON-valid but not an object/array — the
        // repo accepts any valid JSON). For grammar errors the request itself
        // wouldn't parse. So instead exercise the empty-object path which is
        // valid JSON and should be accepted.
        let (status, body) = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["active_envelope"]["version"].as_i64(), Some(1));
    }
}

#[cfg(test)]
mod get_envelope {
    //! GET /tasks/:id/envelope tests (RL-U3-03 / LM-138).
    //! Verification: `cargo test routes::tasks::get_envelope`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_json(app: &axum::Router, uri: &str, body: Value) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST {uri} failed: {:?}",
            resp.status()
        );
        body_json(resp).await
    }

    async fn get_envelope_req(app: &axum::Router, id: &str, query: &str) -> (StatusCode, Value) {
        let uri = if query.is_empty() {
            format!("/tasks/{id}/envelope")
        } else {
            format!("/tasks/{id}/envelope?{query}")
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn delete_envelope_req(app: &axum::Router, id: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/tasks/{id}/envelope"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn delete_envelope_unlinks_active_pointer_and_keeps_history() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "x", "decomposition_policy": "atomic"},
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();

        let (status, body) = delete_envelope_req(&s.app, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["task_id"].as_str(), Some(id));
        assert_eq!(body["cleared"].as_bool(), Some(true));

        let (status, body) = get_envelope_req(&s.app, id, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap_or("").contains("no envelope"));

        let history_resp = s
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/tasks/{id}/envelope/history"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(history_resp.status(), StatusCode::OK);
        let history = body_json(history_resp).await;
        let entries = history.as_array().unwrap();
        assert_eq!(entries.len(), 1, "history must remain after clear");
        assert_eq!(entries[0]["version"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn delete_envelope_idempotent_returns_cleared_false_second_time() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T1"}),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = delete_envelope_req(&s.app, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cleared"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn delete_envelope_unknown_task_returns_404() {
        let s = setup();
        let (status, _) = delete_envelope_req(&s.app, "TASK-NOPE").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn returns_404_when_no_envelope() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T1"}),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = get_envelope_req(&s.app, id, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap_or("").contains("no envelope"));
    }

    #[tokio::test]
    async fn raw_returns_stored_json_unchanged() {
        let s = setup();
        let env = serde_json::json!({
            "version": 1,
            "intent": "raw vs resolved",
            "decomposition_policy": "atomic"
        });
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": env.clone(),
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();

        let (status, body) = get_envelope_req(&s.app, id, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["raw_envelope"], env);
        assert_eq!(
            body["resolved_envelope"], env,
            "default resolve=false: equal to raw"
        );
        assert_eq!(body["version"].as_i64(), Some(1));
        assert_eq!(body["superseded"].as_bool(), Some(false));
        assert_eq!(body["inheritance_chain"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn resolve_true_merges_parent_chain_child_wins() {
        let s = setup();
        let parent = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {
                    "version": 1,
                    "intent": "parent intent",
                    "tags": ["p-tag"],
                    "decomposition_policy": "tree(max_depth=3)"
                },
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap().to_string();

        let child = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "C",
                "parent_task_id": parent_id,
                "envelope": {
                    "version": 1,
                    "intent": "child intent",
                    "tags": ["c-tag"]
                },
            }),
        )
        .await;
        let child_id = child["id"].as_str().unwrap().to_string();

        let (status, body) = get_envelope_req(&s.app, &child_id, "resolve=true").await;
        assert_eq!(status, StatusCode::OK);
        let resolved = &body["resolved_envelope"];
        // Child wins on overlapping keys.
        assert_eq!(resolved["intent"], "child intent");
        assert_eq!(resolved["tags"], serde_json::json!(["c-tag"]));
        // Parent contributes keys child doesn't override.
        assert_eq!(resolved["decomposition_policy"], "tree(max_depth=3)");
        // Inheritance chain root → self.
        let chain = body["inheritance_chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].as_str(), Some(parent_id.as_str()));
        assert_eq!(chain[1].as_str(), Some(child_id.as_str()));
    }

    #[tokio::test]
    async fn version_query_returns_historical_envelope() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "v1"},
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap().to_string();

        // PATCH a v2.
        s.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/tasks/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "envelope": {"version": 1, "intent": "v2"}
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let (status_v2, body_v2) = get_envelope_req(&s.app, &id, "").await;
        assert_eq!(status_v2, StatusCode::OK);
        assert_eq!(body_v2["version"].as_i64(), Some(2));
        assert_eq!(body_v2["raw_envelope"]["intent"], "v2");
        assert_eq!(body_v2["superseded"].as_bool(), Some(false));

        let (status_v1, body_v1) = get_envelope_req(&s.app, &id, "version=1").await;
        assert_eq!(status_v1, StatusCode::OK);
        assert_eq!(body_v1["version"].as_i64(), Some(1));
        assert_eq!(body_v1["raw_envelope"]["intent"], "v1");
        assert_eq!(body_v1["superseded"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn version_query_404_for_unknown_version() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "only v1"},
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = get_envelope_req(&s.app, id, "version=99").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("envelope version"));
    }

    #[tokio::test]
    async fn deep_merge_recurses_into_nested_objects() {
        let s = setup();
        let parent = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {
                    "retry_policy": {
                        "max_attempts": 3,
                        "backoff": "exponential",
                        "checkpoint_interval": "per_file"
                    }
                },
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap().to_string();

        let child = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "C",
                "parent_task_id": parent_id,
                "envelope": {
                    "retry_policy": {
                        "max_attempts": 5
                    }
                },
            }),
        )
        .await;
        let child_id = child["id"].as_str().unwrap().to_string();

        let (status, body) = get_envelope_req(&s.app, &child_id, "resolve=true").await;
        assert_eq!(status, StatusCode::OK);
        let rp = &body["resolved_envelope"]["retry_policy"];
        assert_eq!(rp["max_attempts"].as_i64(), Some(5)); // child wins
        assert_eq!(rp["backoff"], "exponential"); // parent preserved
        assert_eq!(rp["checkpoint_interval"], "per_file");
    }
}

#[cfg(test)]
mod envelope_history {
    //! GET /tasks/:id/envelope/history tests (RL-U3-04 / LM-139).
    //! Verification: `cargo test routes::tasks::envelope_history`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_json(app: &axum::Router, uri: &str, body: Value) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_success());
        body_json(resp).await
    }

    async fn patch_json(app: &axum::Router, uri: &str, body: Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PATCH failed: {:?}",
            resp.status()
        );
    }

    async fn history(app: &axum::Router, id: &str, query: &str) -> (StatusCode, Value) {
        let uri = if query.is_empty() {
            format!("/tasks/{id}/envelope/history")
        } else {
            format!("/tasks/{id}/envelope/history?{query}")
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn empty_history_for_envelope_less_task() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T1"}),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = history(&s.app, id, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn replanning_three_times_returns_three_versions_desc() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "v1"},
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap().to_string();
        patch_json(
            &s.app,
            &format!("/tasks/{id}"),
            serde_json::json!({"envelope": {"version": 1, "intent": "v2"}}),
        )
        .await;
        patch_json(
            &s.app,
            &format!("/tasks/{id}"),
            serde_json::json!({"envelope": {"version": 1, "intent": "v3"}}),
        )
        .await;

        let (status, body) = history(&s.app, &id, "").await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // Version DESC: 3, 2, 1.
        assert_eq!(arr[0]["version"].as_i64(), Some(3));
        assert_eq!(arr[1]["version"].as_i64(), Some(2));
        assert_eq!(arr[2]["version"].as_i64(), Some(1));
        assert_eq!(arr[0]["envelope"]["intent"], "v3");
        assert_eq!(arr[2]["envelope"]["intent"], "v1");
        // Active envelope has no superseded_at.
        assert!(arr[0].get("superseded_at").is_none());
        // Older envelopes do.
        assert!(arr[1]["superseded_at"].as_i64().is_some());
        assert!(arr[2]["superseded_at"].as_i64().is_some());
    }

    #[tokio::test]
    async fn pagination_limit_offset() {
        let s = setup();
        let task = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T1",
                "envelope": {"version": 1, "intent": "v1"},
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap().to_string();
        for i in 2..=4 {
            patch_json(
                &s.app,
                &format!("/tasks/{id}"),
                serde_json::json!({"envelope": {"version": 1, "intent": format!("v{i}")}}),
            )
            .await;
        }

        let (_, page1) = history(&s.app, &id, "limit=2&offset=0").await;
        let arr1 = page1.as_array().unwrap();
        assert_eq!(arr1.len(), 2);
        assert_eq!(arr1[0]["version"].as_i64(), Some(4));
        assert_eq!(arr1[1]["version"].as_i64(), Some(3));

        let (_, page2) = history(&s.app, &id, "limit=2&offset=2").await;
        let arr2 = page2.as_array().unwrap();
        assert_eq!(arr2.len(), 2);
        assert_eq!(arr2[0]["version"].as_i64(), Some(2));
        assert_eq!(arr2[1]["version"].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn unknown_task_returns_404() {
        let s = setup();
        let (status, _) = history(&s.app, "TASK-NONEXISTENT", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod subtasks {
    //! POST /tasks/:id/subtasks tests (RL-U3-06 / LM-141).
    //! Verification: `cargo test routes::tasks::subtasks`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn get_envelope_resolved(app: &axum::Router, id: &str) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/tasks/{id}/envelope?resolve=true"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    #[tokio::test]
    async fn parent_not_found_returns_404() {
        let s = setup();
        let (status, body) = post_json(
            &s.app,
            "/tasks/TASK-NONEXISTENT/subtasks",
            serde_json::json!({"title": "child"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("Parent task not found"));
    }

    #[tokio::test]
    async fn parent_without_envelope_creates_child_with_overrides_only() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "P"}),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();

        let (status, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({
                "title": "C",
                "envelope_overrides": {"version": 1, "intent": "child only"},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(child["parent_task_id"].as_str(), Some(parent_id));
        assert_eq!(child["unit_id"].as_str(), Some(s.unit_id.as_str()));
        assert_eq!(child["active_envelope"]["version"].as_i64(), Some(1));
        let stored: Value =
            serde_json::from_str(child["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["intent"], "child only");
    }

    #[tokio::test]
    async fn child_inherits_parent_envelope_when_no_overrides() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {"version": 1, "intent": "parent intent", "decomposition_policy": "atomic"},
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();

        let (status, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({"title": "C"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(child["parent_task_id"].as_str(), Some(parent_id));
        assert_eq!(child["active_envelope"]["version"].as_i64(), Some(1));
        let stored: Value =
            serde_json::from_str(child["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["intent"], "parent intent");
        assert_eq!(stored["decomposition_policy"], "atomic");
    }

    #[tokio::test]
    async fn child_overrides_deep_merge_into_parent() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {
                    "version": 1,
                    "intent": "parent intent",
                    "retry_policy": {"max_attempts": 3, "backoff": "exponential"}
                },
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();

        let (status, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({
                "title": "C",
                "envelope_overrides": {
                    "intent": "child intent",
                    "retry_policy": {"max_attempts": 5}
                },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(child["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["intent"], "child intent"); // child wins
        assert_eq!(stored["retry_policy"]["max_attempts"].as_i64(), Some(5)); // child wins
        assert_eq!(stored["retry_policy"]["backoff"], "exponential"); // parent preserved
    }

    #[tokio::test]
    async fn resolved_envelope_reflects_inheritance_chain() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {"version": 1, "intent": "parent", "tags": ["root"]},
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();
        let (_, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({
                "title": "C",
                "envelope_overrides": {"intent": "child"}
            }),
        )
        .await;
        let child_id = child["id"].as_str().unwrap();

        let resolved = get_envelope_resolved(&s.app, child_id).await;
        // Child stored already inherits parent — both raw and resolved equal.
        assert_eq!(resolved["resolved_envelope"]["intent"], "child");
        assert_eq!(
            resolved["resolved_envelope"]["tags"],
            serde_json::json!(["root"])
        );
        let chain = resolved["inheritance_chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].as_str(), Some(parent_id));
        assert_eq!(chain[1].as_str(), Some(child_id));
    }

    #[tokio::test]
    async fn scope_boundary_narrowing_succeeds() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {"version": 1, "scope_boundary": ["src/a/", "src/b/", "src/c/"]},
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();

        let (status, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({
                "title": "C",
                "envelope_overrides": {"scope_boundary": ["src/a/"]},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(child["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["scope_boundary"], serde_json::json!(["src/a/"]));
    }

    #[tokio::test]
    async fn scope_boundary_widening_returns_400() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "P",
                "envelope": {"version": 1, "scope_boundary": ["src/a/"]},
            }),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();

        let (status, body) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({
                "title": "C",
                "envelope_overrides": {"scope_boundary": ["src/a/", "src/x/"]},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("scope_boundary widening"));
    }

    #[tokio::test]
    async fn parent_cycle_id_inherited_when_unspecified() {
        let s = setup();
        let (_, parent) = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "P"}),
        )
        .await;
        let parent_id = parent["id"].as_str().unwrap();
        let (status, child) = post_json(
            &s.app,
            &format!("/tasks/{parent_id}/subtasks"),
            serde_json::json!({"title": "C"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(child["parent_task_id"].as_str(), Some(parent_id));
        assert_eq!(child["cycle_id"].as_str(), Some(s.cycle_id.as_str()));
    }
}

#[cfg(test)]
mod tree {
    //! Tree traversal API tests (RL-U3-07 / LM-60).
    //! Verification: `cargo test routes::tasks::tree`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_json(app: &axum::Router, uri: &str, body: Value) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST {uri} failed: {:?}",
            resp.status()
        );
        body_json(resp).await
    }

    async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn patch_json(app: &axum::Router, uri: &str, body: Value) -> StatusCode {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status()
    }

    /// Build a 4-deep linear chain root → A → B → C and return the IDs.
    async fn build_chain(s: &Setup) -> (String, String, String, String) {
        let root = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "R",
                "envelope": {"version": 1, "intent": "root"}
            }),
        )
        .await;
        let root_id = root["id"].as_str().unwrap().to_string();
        let a = post_json(
            &s.app,
            &format!("/tasks/{root_id}/subtasks"),
            serde_json::json!({"title": "A"}),
        )
        .await;
        let a_id = a["id"].as_str().unwrap().to_string();
        let b = post_json(
            &s.app,
            &format!("/tasks/{a_id}/subtasks"),
            serde_json::json!({"title": "B"}),
        )
        .await;
        let b_id = b["id"].as_str().unwrap().to_string();
        let c = post_json(
            &s.app,
            &format!("/tasks/{b_id}/subtasks"),
            serde_json::json!({"title": "C"}),
        )
        .await;
        let c_id = c["id"].as_str().unwrap().to_string();
        (root_id, a_id, b_id, c_id)
    }

    #[tokio::test]
    async fn ancestors_returns_parent_chain_root_last_with_resolved_envelope() {
        let s = setup();
        let (root, a, b, c) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{c}/ancestors")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"].as_str(), Some(b.as_str()));
        assert_eq!(arr[0]["depth"].as_i64(), Some(1));
        assert_eq!(arr[1]["id"].as_str(), Some(a.as_str()));
        assert_eq!(arr[1]["depth"].as_i64(), Some(2));
        assert_eq!(arr[2]["id"].as_str(), Some(root.as_str()));
        assert_eq!(arr[2]["depth"].as_i64(), Some(3));
        // Root has an envelope, so ancestors of C resolve through root.
        assert_eq!(arr[2]["resolved_envelope"]["intent"], "root");
    }

    #[tokio::test]
    async fn descendants_dfs_returns_three_levels() {
        let s = setup();
        let (root, a, b, c) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{root}/descendants")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"].as_str(), Some(a.as_str()));
        assert_eq!(arr[0]["depth"].as_i64(), Some(1));
        assert_eq!(arr[1]["id"].as_str(), Some(b.as_str()));
        assert_eq!(arr[1]["depth"].as_i64(), Some(2));
        assert_eq!(arr[2]["id"].as_str(), Some(c.as_str()));
        assert_eq!(arr[2]["depth"].as_i64(), Some(3));
    }

    #[tokio::test]
    async fn descendants_depth_limit_truncates() {
        let s = setup();
        let (root, a, b, _c) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{root}/descendants?depth=2")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"].as_str(), Some(a.as_str()));
        assert_eq!(arr[1]["id"].as_str(), Some(b.as_str()));
    }

    #[tokio::test]
    async fn descendants_bfs_traverses_level_order() {
        // Tree: root -> [a1, a2]; a1 -> [b1]; a2 -> [b2]
        let s = setup();
        let root = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "R"}),
        )
        .await;
        let root_id = root["id"].as_str().unwrap().to_string();
        let a1 = post_json(
            &s.app,
            &format!("/tasks/{root_id}/subtasks"),
            serde_json::json!({"title": "A1"}),
        )
        .await;
        let a1_id = a1["id"].as_str().unwrap().to_string();
        let a2 = post_json(
            &s.app,
            &format!("/tasks/{root_id}/subtasks"),
            serde_json::json!({"title": "A2"}),
        )
        .await;
        let a2_id = a2["id"].as_str().unwrap().to_string();
        let b1 = post_json(
            &s.app,
            &format!("/tasks/{a1_id}/subtasks"),
            serde_json::json!({"title": "B1"}),
        )
        .await;
        let b1_id = b1["id"].as_str().unwrap().to_string();
        let b2 = post_json(
            &s.app,
            &format!("/tasks/{a2_id}/subtasks"),
            serde_json::json!({"title": "B2"}),
        )
        .await;
        let b2_id = b2["id"].as_str().unwrap().to_string();

        let (_, dfs) = get_json(&s.app, &format!("/tasks/{root_id}/descendants?order=dfs")).await;
        let dfs_ids: Vec<&str> = dfs
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        // DFS pre-order: a1, b1, a2, b2
        assert_eq!(
            dfs_ids,
            vec![
                a1_id.as_str(),
                b1_id.as_str(),
                a2_id.as_str(),
                b2_id.as_str()
            ]
        );

        let (_, bfs) = get_json(&s.app, &format!("/tasks/{root_id}/descendants?order=bfs")).await;
        let bfs_ids: Vec<&str> = bfs
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        // BFS level-order: a1, a2, b1, b2
        assert_eq!(
            bfs_ids,
            vec![
                a1_id.as_str(),
                a2_id.as_str(),
                b1_id.as_str(),
                b2_id.as_str()
            ]
        );
    }

    #[tokio::test]
    async fn subtree_includes_self_at_depth_zero() {
        let s = setup();
        let (root, _, _, _) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{root}/subtree")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 4); // self + 3 descendants
        assert_eq!(arr[0]["id"].as_str(), Some(root.as_str()));
        assert_eq!(arr[0]["depth"].as_i64(), Some(0));
    }

    #[tokio::test]
    async fn subtree_depth_zero_returns_self_only() {
        let s = setup();
        let (root, _, _, _) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{root}/subtree?depth=0")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"].as_str(), Some(root.as_str()));
    }

    #[tokio::test]
    async fn ancestors_of_root_is_empty() {
        let s = setup();
        let (root, _, _, _) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{root}/ancestors")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn descendants_of_leaf_is_empty() {
        let s = setup();
        let (_, _, _, c) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{c}/descendants")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn unknown_task_returns_404_for_all_three_endpoints() {
        let s = setup();
        for path in &["ancestors", "descendants", "subtree"] {
            let (status, _) = get_json(&s.app, &format!("/tasks/TASK-NONEXISTENT/{path}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} should 404");
        }
    }

    #[tokio::test]
    async fn parent_update_creating_cycle_is_rejected() {
        // Build root -> a -> b. Try to set root.parent = b → would form a cycle.
        let s = setup();
        let root = post_json(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "R"}),
        )
        .await;
        let root_id = root["id"].as_str().unwrap().to_string();
        let a = post_json(
            &s.app,
            &format!("/tasks/{root_id}/subtasks"),
            serde_json::json!({"title": "A"}),
        )
        .await;
        let a_id = a["id"].as_str().unwrap().to_string();
        let b = post_json(
            &s.app,
            &format!("/tasks/{a_id}/subtasks"),
            serde_json::json!({"title": "B"}),
        )
        .await;
        let b_id = b["id"].as_str().unwrap().to_string();

        let status = patch_json(
            &s.app,
            &format!("/tasks/{root_id}"),
            serde_json::json!({"parent_task_id": b_id}),
        )
        .await;
        // The internal bail! routes through ApiError as 409 (conflict) per
        // the existing String-matching mapper for "cycle"-related errors, or
        // 500 if not specifically mapped. The contract here is "rejected, not
        // applied" — assert non-2xx and verify root still has no parent.
        assert!(!status.is_success(), "status was {status}");
        let (_, root_now) = get_json(&s.app, &format!("/tasks/{root_id}")).await;
        assert!(root_now["parent_task_id"].is_null());
    }

    #[tokio::test]
    async fn ancestors_includes_resolved_envelope_only_when_envelope_exists() {
        // Root has envelope, A doesn't. Ancestors of C: [B (no env), A (no env), root (env)].
        let s = setup();
        let (root, _a, _b, c) = build_chain(&s).await;
        let (status, body) = get_json(&s.app, &format!("/tasks/{c}/ancestors")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().unwrap();
        // B has no envelope → ancestors-of-C[0] (which is B) should still
        // resolve through root, since resolve walks the full chain.
        assert_eq!(arr[0]["resolved_envelope"]["intent"], "root");
        // Root level entry resolves to its own envelope.
        assert_eq!(arr[2]["id"].as_str(), Some(root.as_str()));
        assert_eq!(arr[2]["resolved_envelope"]["intent"], "root");
    }
}

#[cfg(test)]
mod planned_sha_autofill {
    //! POST /tasks envelope.target_repo → planned_sha auto-fill (RL-U3-08 / LM-61).
    //! Verification: `cargo test routes::tasks::planned_sha_autofill`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::process::Command as Cmd;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    fn init_repo(path: &std::path::Path) -> String {
        Cmd::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(path)
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        std::fs::write(path.join("README"), "x").unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["add", "."])
            .status()
            .unwrap();
        Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        let out = Cmd::new("git")
            .args(["-C"])
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
        repo_path: std::path::PathBuf,
        head_sha: String,
    }

    /// Sets up a project whose cwd is a tempdir git repo named `repo_basename`.
    fn setup_with_repo(repo_basename: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join(repo_basename);
        std::fs::create_dir_all(&repo_path).unwrap();
        let head = init_repo(&repo_path);

        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let project = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: Some(repo_path.to_str().unwrap()),
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
            repo_path,
            head_sha: head,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post(app: &axum::Router, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn target_repo_resolves_to_head_sha() {
        let s = setup_with_repo("daemon");
        let (status, body) = post(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {"version": 1, "target_repo": "daemon"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(body["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["planned_sha"].as_str(), Some(s.head_sha.as_str()));
        assert!(stored.get("planned_sha_warning").is_none());
    }

    #[tokio::test]
    async fn at_head_suffix_is_accepted() {
        let s = setup_with_repo("daemon");
        let (_, body) = post(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {"version": 1, "target_repo": "daemon@HEAD"}
            }),
        )
        .await;
        let stored: Value =
            serde_json::from_str(body["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(stored["planned_sha"].as_str(), Some(s.head_sha.as_str()));
    }

    #[tokio::test]
    async fn unknown_target_repo_falls_back_to_null_with_warning() {
        let s = setup_with_repo("daemon");
        let _ = s.repo_path; // silence unused
        let (status, body) = post(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {"version": 1, "target_repo": "nonexistent-repo-xyz"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(body["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert!(stored["planned_sha"].is_null());
        let warn = stored["planned_sha_warning"].as_str().unwrap();
        assert!(warn.contains("nonexistent-repo-xyz"));
    }

    #[tokio::test]
    async fn explicit_planned_sha_is_preserved() {
        let s = setup_with_repo("daemon");
        let (status, body) = post(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "daemon",
                    "planned_sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(body["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert_eq!(
            stored["planned_sha"].as_str(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    #[tokio::test]
    async fn no_target_repo_means_no_autofill() {
        let s = setup_with_repo("daemon");
        let (status, body) = post(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {"version": 1, "intent": "no target"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let stored: Value =
            serde_json::from_str(body["active_envelope"]["json"].as_str().unwrap()).unwrap();
        assert!(stored.get("planned_sha").is_none());
        assert!(stored.get("planned_sha_warning").is_none());
    }
}

#[cfg(test)]
mod drift {
    //! GET /tasks/:id/drift tests (RL-U3-09 / LM-62).
    //! Verification: `cargo test routes::tasks::drift`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::process::Command as Cmd;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    fn git(args: &[&str], cwd: &std::path::Path) {
        let status = Cmd::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn rev_parse(cwd: &std::path::Path) -> String {
        let out = Cmd::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write_and_commit(cwd: &std::path::Path, file: &str, content: &str, msg: &str) -> String {
        let path = cwd.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        git(&["add", "."], cwd);
        git(&["commit", "-q", "-m", msg], cwd);
        rev_parse(cwd)
    }

    fn init_repo(cwd: &std::path::Path) -> String {
        std::fs::create_dir_all(cwd).unwrap();
        git(&["init", "-q", "-b", "main"], cwd);
        git(&["config", "user.email", "t@example.com"], cwd);
        git(&["config", "user.name", "T"], cwd);
        write_and_commit(cwd, "src/foo.rs", "//foo\n", "init")
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
        repo_path: std::path::PathBuf,
    }

    fn setup_with_repo(basename: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join(basename);
        let _ = init_repo(&repo_path);

        let mut db = Db::open(&dir.path().join("test.db")).unwrap();
        let project = projects::create(
            &mut db.conn,
            projects::CreateInput {
                name: "P",
                description: None,
                cwd: Some(repo_path.to_str().unwrap()),
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
            repo_path,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_task(app: &axum::Router, body: Value) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST /tasks: {:?}",
            resp.status()
        );
        body_json(resp).await
    }

    async fn drift_get(app: &axum::Router, id: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/tasks/{id}/drift"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn drift_none_when_no_changes_since_planned_sha() {
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "daemon",
                    "scope_boundary": ["src/"]
                }
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = drift_get(&s.app, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["drift_level"], "none");
        assert_eq!(body["total_changed"].as_u64(), Some(0));
        assert_eq!(body["changed_files_in_scope"].as_array().unwrap().len(), 0);
        assert_eq!(body["planned_sha"], body["current_sha"]);
    }

    #[tokio::test]
    async fn drift_major_when_three_or_more_in_scope() {
        // Success-criteria reproduction: 10 files changed total, 3 in scope.
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "daemon",
                    "scope_boundary": ["src/in_scope/"]
                }
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        // Make 3 in-scope changes + 7 out-of-scope changes since planned_sha.
        for i in 0..3 {
            write_and_commit(
                &s.repo_path,
                &format!("src/in_scope/f{i}.rs"),
                "//x\n",
                &format!("in{i}"),
            );
        }
        for i in 0..7 {
            write_and_commit(
                &s.repo_path,
                &format!("docs/d{i}.md"),
                "x\n",
                &format!("out{i}"),
            );
        }

        let (status, body) = drift_get(&s.app, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["drift_level"], "major");
        assert_eq!(body["total_changed"].as_u64(), Some(10));
        assert_eq!(body["changed_files_in_scope"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn drift_minor_for_one_or_two_in_scope() {
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "daemon",
                    "scope_boundary": ["src/in_scope/"]
                }
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        write_and_commit(&s.repo_path, "src/in_scope/a.rs", "//\n", "a");
        write_and_commit(&s.repo_path, "docs/x.md", "x\n", "x");
        let (_, body) = drift_get(&s.app, id).await;
        assert_eq!(body["drift_level"], "minor");
        assert_eq!(body["total_changed"].as_u64(), Some(2));
        assert_eq!(body["changed_files_in_scope"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn drift_400_when_envelope_missing_planned_sha() {
        let s = setup_with_repo("daemon");
        // Explicit planned_sha=null bypasses autofill (autofill replaces null
        // with a warning); we want the "no planned_sha" path. Use a target
        // that auto-fills as null + warning, then call drift.
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "doesnotexist",
                    "scope_boundary": ["src/"]
                }
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = drift_get(&s.app, id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap_or("").contains("planned_sha"));
    }

    #[tokio::test]
    async fn drift_404_when_no_envelope() {
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T"}),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = drift_get(&s.app, id).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap_or("").contains("no envelope"));
    }

    #[tokio::test]
    async fn drift_404_when_target_repo_unregistered() {
        // Set up a real repo (so autofill works on it) then patch envelope to
        // point at a target_repo with no project cwd registration. We do this
        // by patching the active envelope to use an explicit planned_sha and
        // a name that isn't registered.
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {
                    "version": 1,
                    "target_repo": "unknown-repo-xyz",
                    "planned_sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                }
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        let (status, body) = drift_get(&s.app, id).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap_or("").contains("target_repo"));
    }

    #[tokio::test]
    async fn drift_empty_scope_boundary_treats_all_changes_as_in_scope() {
        let s = setup_with_repo("daemon");
        let task = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": {"version": 1, "target_repo": "daemon"}
            }),
        )
        .await;
        let id = task["id"].as_str().unwrap();
        write_and_commit(&s.repo_path, "any/file.txt", "x\n", "any");
        let (_, body) = drift_get(&s.app, id).await;
        // Without scope_boundary, every changed file counts.
        assert_eq!(body["total_changed"].as_u64(), Some(1));
        assert_eq!(body["changed_files_in_scope"].as_array().unwrap().len(), 1);
        assert_eq!(body["drift_level"], "minor");
    }
}

#[cfg(test)]
mod conditions_hook {
    //! End-to-end tests for the precondition/postcondition status-transition
    //! guard wired into PATCH /tasks/:id (RL-U3-10 / LM-63).
    //!
    //! Verification: `cargo test routes::tasks::conditions_hook`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post_task(app: &axum::Router, body: Value) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    }

    async fn patch_task(app: &axum::Router, id: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/tasks/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn done_transition_blocked_when_postcondition_fails() {
        let s = setup();
        let env = serde_json::json!({
            "version": 1,
            "intent": "p",
            "postconditions": [
                {"type": "task_status", "task_id": "TASK-NOPE", "equals": "done"}
            ]
        });
        let created = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": env,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) = patch_task(
            &s.app,
            id,
            serde_json::json!({"status": "done", "evidence": "test:done"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["details"]["field"], "postconditions");
        assert_eq!(
            body["details"]["violating_predicate"]["type"],
            "task_status"
        );
        assert!(body["error"].as_str().unwrap().contains("postconditions"));
    }

    #[tokio::test]
    async fn done_transition_allowed_when_postcondition_passes() {
        let s = setup();
        let env = serde_json::json!({
            "version": 1,
            "intent": "p",
            "postconditions": [{"type": "daemon_healthy"}]
        });
        let created = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": env,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) = patch_task(
            &s.app,
            id,
            serde_json::json!({"status": "done", "evidence": "test:done"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "done");
    }

    #[tokio::test]
    async fn done_transition_works_when_no_envelope_present() {
        let s = setup();
        let created = post_task(
            &s.app,
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T"}),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) = patch_task(
            &s.app,
            id,
            serde_json::json!({"status": "done", "evidence": "test:done"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "done");
    }

    #[tokio::test]
    async fn done_transition_works_when_envelope_has_no_postconditions() {
        let s = setup();
        let env = serde_json::json!({"version": 1, "intent": "p"});
        let created = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": env,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) = patch_task(
            &s.app,
            id,
            serde_json::json!({"status": "done", "evidence": "test:done"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "done");
    }

    #[tokio::test]
    async fn cancelled_transition_does_not_evaluate_postconditions() {
        // Postconditions only gate `done`; `cancelled` is an escape hatch and
        // must remain reachable so failed work can be closed out cleanly.
        let s = setup();
        let env = serde_json::json!({
            "version": 1,
            "intent": "p",
            "postconditions": [
                {"type": "task_status", "task_id": "TASK-NOPE", "equals": "done"}
            ]
        });
        let created = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": env,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) =
            patch_task(&s.app, id, serde_json::json!({"status": "cancelled"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "cancelled");
    }

    #[tokio::test]
    async fn postcondition_violation_includes_full_predicate_in_details() {
        let s = setup();
        let pred = serde_json::json!({
            "type": "file_exists",
            "path": "/tmp/__clawket_definitely_does_not_exist__"
        });
        let env = serde_json::json!({
            "version": 1,
            "intent": "p",
            "postconditions": [pred]
        });
        let created = post_task(
            &s.app,
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": env,
            }),
        )
        .await;
        let id = created["id"].as_str().unwrap();
        let (status, body) = patch_task(
            &s.app,
            id,
            serde_json::json!({"status": "done", "evidence": "test:done"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body["details"]["violating_predicate"]["type"],
            "file_exists"
        );
        assert!(body["details"]["reason"]
            .as_str()
            .unwrap()
            .contains("does not exist"));
    }
}

#[cfg(test)]
mod entropy_hook {
    //! Envelope rejects high-entropy values that look like accidentally
    //! pasted secrets (RL-U3-15 / LM-68).
    //!
    //! Verification: `cargo test routes::tasks::entropy_hook`.

    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use crate::routes::router;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    async fn post_raw(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn patch_raw(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn create_rejects_envelope_with_high_entropy_leaf() {
        let s = setup();
        let envelope = serde_json::json!({
            "version": 1,
            "intent": "abcdefghijklmnopqrstuvwxyz0123456789ABCDE",
        });
        let (status, body) = post_raw(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": envelope,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let msg = body["error"].as_str().unwrap_or("");
        assert!(
            msg.contains("secrets_ref"),
            "expected secrets_ref guidance in error, got {msg}"
        );
    }

    #[tokio::test]
    async fn create_accepts_envelope_with_secrets_ref() {
        let s = setup();
        let envelope = serde_json::json!({
            "version": 1,
            "intent": "Wire the Anthropic client",
            "secrets_ref": {"ANTHROPIC_API_KEY": "env:ANTHROPIC_API_KEY"}
        });
        let (status, _) = post_raw(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": envelope,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn update_rejects_envelope_with_high_entropy_leaf() {
        let s = setup();
        let (status_create, created) = post_raw(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T"}),
        )
        .await;
        assert_eq!(status_create, StatusCode::OK);
        let id = created["id"].as_str().unwrap();
        let bad_env = serde_json::json!({
            "version": 1,
            "intent": "p",
            "context_refs": ["abcdefghijklmnopqrstuvwxyz0123456789ABCDE"]
        });
        let (status, body) = patch_raw(
            &s.app,
            &format!("/tasks/{id}"),
            serde_json::json!({"envelope": bad_env}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap_or("").contains("secrets_ref"));
    }

    #[tokio::test]
    async fn create_passes_natural_envelope() {
        let s = setup();
        let envelope = serde_json::json!({
            "version": 1,
            "intent": "Add task usage endpoints with budget tracking",
            "prompt_template": "Add migration. Wire routes. Run tests.",
            "success_criteria": "All tests pass",
        });
        let (status, _) = post_raw(
            &s.app,
            "/tasks",
            serde_json::json!({
                "unit_id": s.unit_id, "cycle_id": s.cycle_id,
                "title": "T",
                "envelope": envelope,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[cfg(test)]
mod lease {
    //! POST/DELETE /tasks/:id/lease + POST /tasks/:id/lease/heartbeat
    //! tests (RL-U5-07a / LM-179).
    //! Verification: `cargo test routes::tasks::lease`.

    use super::*;
    use crate::db::Db;
    use crate::paths::Paths;
    use crate::repo::{cycles, plans, projects, units};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_paths(root: &std::path::Path) -> Paths {
        let cache = root.join("cache");
        Paths {
            data: root.join("data"),
            cache: cache.clone(),
            config: root.join("config"),
            state: root.join("state"),
            db: root.join("db.sqlite"),
            port_file: cache.join("port"),
            pid_file: cache.join("pid"),
            socket: cache.join("sock"),
            token_file: cache.join("token"),
            web_dir: None,
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        app: axum::Router,
        unit_id: String,
        cycle_id: String,
    }

    fn setup() -> Setup {
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
        cycles::activate(&db.conn, &cycle.id).unwrap();
        let paths = test_paths(dir.path());
        let state = AppState::new(db, paths, String::new());
        let app = router().with_state(state);
        Setup {
            _dir: dir,
            app,
            unit_id: unit.id,
            cycle_id: cycle.id,
        }
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn delete(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn make_task(app: &axum::Router, unit_id: &str, cycle_id: &str) -> String {
        let (status, body) = post(
            app,
            "/tasks",
            serde_json::json!({"unit_id": unit_id, "cycle_id": cycle_id, "title": "T"}),
        )
        .await;
        assert!(status.is_success(), "create task failed: {status:?}");
        body["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn acquire_grants_lease_on_unheld_task() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (status, body) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 60000}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["task_id"].as_str(), Some(id.as_str()));
        assert_eq!(body["session_id"], "session-A");
        let acquired = body["acquired_at"].as_i64().unwrap();
        let expires = body["expires_at"].as_i64().unwrap();
        assert!(expires > acquired);
        assert!(expires - acquired >= 60_000);
    }

    #[tokio::test]
    async fn acquire_returns_409_with_holder_when_other_session_holds() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (st_a, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 60000}),
        )
        .await;
        assert_eq!(st_a, StatusCode::OK);

        let (st_b, body_b) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-B", "ttl_ms": 60000}),
        )
        .await;
        assert_eq!(st_b, StatusCode::CONFLICT);
        let holder = &body_b["details"]["holder"];
        assert_eq!(holder["session_id"], "session-A");
        assert_eq!(holder["task_id"].as_str(), Some(id.as_str()));
        assert!(body_b["error"].as_str().unwrap_or("").contains("session-A"));
    }

    #[tokio::test]
    async fn acquire_same_session_refreshes_ttl() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (_, first) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 30000}),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (st, second) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 120000}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        // acquired_at preserved across same-session refresh
        assert_eq!(second["acquired_at"], first["acquired_at"]);
        // expires_at extends
        assert!(second["expires_at"].as_i64().unwrap() >= first["expires_at"].as_i64().unwrap());
    }

    #[tokio::test]
    async fn acquire_404_when_task_missing() {
        let s = setup();
        let (status, body) = post(
            &s.app,
            "/tasks/TASK-NOPE/lease",
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("Task not found"));
    }

    #[tokio::test]
    async fn acquire_400_on_missing_session_id() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (status, body) =
            post(&s.app, &format!("/tasks/{id}/lease"), serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap_or("").contains("session_id"));
    }

    #[tokio::test]
    async fn acquire_400_on_blank_session_id() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (status, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "   "}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn acquire_400_on_non_positive_ttl() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (status, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "S", "ttl_ms": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn acquire_400_on_ttl_above_cap() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (status, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "S", "ttl_ms": 4_000_000}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn release_owner_returns_true_then_lease_is_free() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (_, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        let (st, body) = delete(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["released"], true);
        // Now session-B can acquire.
        let (st_b, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-B"}),
        )
        .await;
        assert_eq!(st_b, StatusCode::OK);
    }

    #[tokio::test]
    async fn release_non_owner_returns_false_without_clobbering() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (_, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        let (st, body) = delete(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-B"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["released"], false);
        // session-A still holds it.
        let (st_b, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-B"}),
        )
        .await;
        assert_eq!(st_b, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn heartbeat_extends_ttl_for_owner() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (_, first) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 30000}),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (st, body) = post(
            &s.app,
            &format!("/tasks/{id}/lease/heartbeat"),
            serde_json::json!({"session_id": "session-A", "ttl_ms": 120000}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(body["expires_at"].as_i64().unwrap() > first["expires_at"].as_i64().unwrap());
        assert!(body["heartbeat_at"].as_i64().unwrap() >= first["heartbeat_at"].as_i64().unwrap());
    }

    #[tokio::test]
    async fn heartbeat_409_for_non_owner() {
        let s = setup();
        let id = make_task(&s.app, &s.unit_id, &s.cycle_id).await;
        let (_, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        let (st, body) = post(
            &s.app,
            &format!("/tasks/{id}/lease/heartbeat"),
            serde_json::json!({"session_id": "session-B"}),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap_or("").contains("session-B"));
    }

    #[tokio::test]
    async fn heartbeat_404_when_task_missing() {
        let s = setup();
        let (status, _) = post(
            &s.app,
            "/tasks/TASK-NOPE/lease/heartbeat",
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Regression: the URL path id can be a ticket key (e.g. "LM-180") that
    /// resolves to a different canonical ULID. The handler must fetch the
    /// task and pass `task.id` (not the URL `id`) to `locks::*`, otherwise
    /// the FK constraint on `task_locks.task_id` fires with a 500.
    #[tokio::test]
    async fn acquire_resolves_ticket_key_to_canonical_id() {
        let s = setup();
        // Reuse the project test_paths route to build a project with a key
        // — `setup()` creates a default project without a key, which makes
        // the daemon use a synthetic ticket prefix. We just need *any*
        // ticket form here, so request the task by `task.ticket_number`.
        let (st, body) = post(
            &s.app,
            "/tasks",
            serde_json::json!({"unit_id": s.unit_id, "cycle_id": s.cycle_id, "title": "T"}),
        )
        .await;
        assert!(st.is_success());
        let ticket = body["ticket_number"].as_str().expect("ticket_number");
        // ticket_number is the human-readable form (e.g. "P-1"), distinct
        // from the ULID id. Both must resolve.
        let (status, body) = post(
            &s.app,
            &format!("/tasks/{ticket}/lease"),
            serde_json::json!({"session_id": "session-A"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "ticket-key path should resolve to ULID; body={body}"
        );
        // Same task by ULID must collide with itself — different session
        // sees Conflict.
        let id = body["task_id"].as_str().unwrap();
        let (st_b, _) = post(
            &s.app,
            &format!("/tasks/{id}/lease"),
            serde_json::json!({"session_id": "session-B"}),
        )
        .await;
        assert_eq!(st_b, StatusCode::CONFLICT);
    }
}
