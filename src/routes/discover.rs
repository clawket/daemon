// discover.rs — /discover-loop API endpoints (DOGFOOD-001~050)
//
// Implements the PDD v3.0 discover-loop automation surface:
//   A. Plan/cycle/unit auto-generation (DOGFOOD-001~010)
//   B. Dispatch batch-size metadata + TSV schema validation (DOGFOOD-011~020)
//   C. Bulk sync transcription: TSV → DB task status (DOGFOOD-021~030)
//   D. 3-way convergence query + next-round determination (DOGFOOD-031~050)
//
// Design constraints (PDD O8):
//   - Never DROP / DELETE / TRUNCATE DB rows.
//   - No reasoning decisions inside sync paths (X9 anti-pattern).
//   - TSV status mapping is transcription-only (pass→done, defect→blocked,
//     scenario_error→cancelled). Both `tasks.status` (workflow) and
//     `tasks.qa_status` (round-result) are written in lock-step so either
//     column is a valid count source for convergence (DOGFOOD-039 fix).
//   - cross-unit cycle allowed (PDD v3.0 A4 rollback — unit_id on cycle
//     points to first unit; tasks in other units share the same cycle_id).
//
// Daemon-vs-agent boundary (R2 DOGFOOD distill, see SKILL.md "Agent-side
// procedures"): the daemon owns deterministic state (DB writes, schema
// validation, convergence math). The skill agent owns orchestration that
// requires LLM judgement (sub-agent dispatch, /scenario-refine invocation,
// audit-artifact composition, attention-dilution interpretation). Each
// "defect" reasoning that points at "no daemon code for X" should be
// re-evaluated against SKILL.md to find the agent-side procedure entry
// — the daemon exposes the primitives, not the entire workflow.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

use crate::id::ulid;
use crate::models::{Cycle, Plan, Unit};
use crate::repo::{cycles, knowledge, plans, tasks, units};
use crate::routes::error::{ApiError, ApiResult};
use crate::routes::util::{norm_opt, resolve_project_ref};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        // A. Plan/cycle/unit auto-generation
        .route("/discover-loop/start", post(start))
        .route("/discover-loop/next-round", post(next_round))
        // B. Dispatch metadata + TSV schema validation
        .route("/discover-loop/dispatch-plan", get(dispatch_plan))
        .route("/discover-loop/verify-tsv", post(verify_tsv))
        .route("/discover-loop/batch-id", post(generate_batch_id))
        // C. Bulk sync transcription
        .route("/discover-loop/sync", post(bulk_sync))
        // D. Convergence query
        .route("/discover-loop/status", get(convergence_status))
        .route("/discover-loop/converged", get(converged))
        // Round-level plans list (DOGFOOD-033)
        .route("/discover-loop/rounds/{project_id}", get(list_round_plans))
}

// ---------------------------------------------------------------------------
// A. Plan/cycle/unit auto-generation
// ---------------------------------------------------------------------------

/// DOGFOOD-001: POST /discover-loop/start
///
/// Auto-generate a round-R plan (approved → active) + active cycle + parallel
/// QA units in a single request. Follows qa-flow §4 naming convention:
///   Plan:  "<domain> Round <R>"
///   Units: "QA-<domain> <area>" with mode=parallel
///   Cycle: "<domain> Round <R> Cycle" (anchored to first unit; cross-unit OK)
#[derive(Deserialize)]
struct StartBody {
    /// Project ID the round belongs to.
    project_id: String,
    /// Human-readable domain name (e.g. "Dogfood", "Chess 학습").
    domain: String,
    /// Round number (1-based). Plan title → "<domain> Round <round>".
    round: u32,
    /// QA unit areas (e.g. ["대시보드", "CLI", "Daemon"]).
    /// Each produces one Unit with execution_mode=parallel.
    unit_areas: Vec<String>,
    /// Optional description for the plan.
    description: Option<String>,
}

#[derive(Serialize)]
struct StartResponse {
    plan: Plan,
    units: Vec<Unit>,
    cycle: Cycle,
}

async fn start(
    State(app): State<AppState>,
    Json(body): Json<StartBody>,
) -> ApiResult<Json<StartResponse>> {
    if body.unit_areas.is_empty() {
        return Err(ApiError::bad_request_coded(
            "MISSING_UNIT_AREAS",
            "MISSING_UNIT_AREAS: unit_areas must have at least one entry",
        ));
    }
    if body.round == 0 {
        return Err(ApiError::bad_request_coded(
            "INVALID_ROUND",
            "INVALID_ROUND: round must be >= 1",
        ));
    }

    let conn = app.conn();
    let project_id = resolve_project_ref(&conn, &body.project_id)?;
    let plan_title = format!("{} Round {}", body.domain, body.round);
    // R2 DOGFOOD-006 fix: auto-populate plan description with PDD C6
    // convergence condition when caller didn't supply one. R3 sub-agents
    // verifying the round plan find the spec inline rather than hunting docs.
    let convergence_text = format!(
        "Round {round} of domain '{domain}' (discover-loop).\n\n\
         수렴 조건 (qa-flow §7, PDD C6):\n\
         - defect = 0 (코드층 수렴)\n\
         - scenario_error = 0 (시나리오층 수렴)\n\
         - 마지막 2 라운드 연속 0 (안전 마진)\n\n\
         Done = 위 3 조건이 모두 만족된 라운드의 plan status=completed.\n\
         미수렴 시 next-round 호출하여 Round {next_round} 생성.",
        round = body.round,
        domain = body.domain,
        next_round = body.round + 1,
    );
    let description = norm_opt(body.description).or(Some(convergence_text));

    // R2 DOGFOOD-009 fix: emit a structured warning when active plan count is
    // already > 1 for this project (PDD recommends ≤ 1 active plan; transition
    // periods may have ≤ 2 — anything beyond that is a decomposition smell).
    if let Ok(existing_active) = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project_id),
            status: Some("active"),
        },
    ) {
        let count = existing_active.len();
        if count >= 1 {
            tracing::warn!(
                project_id = %project_id,
                active_plan_count = count,
                next_round = body.round,
                "discover-loop:start — {} active plan(s) exist; PDD recommends ≤ 1 (≤ 2 in transition).",
                count + 1
            );
            app.emit(
                "discover-loop:active-plan-warning",
                serde_json::json!({
                    "project_id": project_id,
                    "active_plan_count_before": count,
                    "active_plan_count_after": count + 1,
                    "recommended_max": 1,
                    "transition_max": 2,
                }),
            );
        }
    }

    // Create plan (draft) then approve it (→ active). Tasks require active plan.
    let plan = plans::create(
        &conn,
        plans::CreateInput {
            project_id: &project_id,
            title: &plan_title,
            description: description.as_deref(),
            source: Some("discover-loop"),
            source_path: None,
            auto_advance: false,
        },
    )?
    .ok_or_else(|| ApiError::internal("plan creation returned None"))?;

    let plan = plans::approve(&conn, &plan.id)?
        .ok_or_else(|| ApiError::internal("plan approve returned None"))?;

    // Create QA units in parallel mode.
    let mut created_units: Vec<Unit> = Vec::with_capacity(body.unit_areas.len());
    for (idx, area) in body.unit_areas.iter().enumerate() {
        let unit_title = format!("QA-{} {}", body.domain, area);
        let u = units::create(
            &conn,
            units::CreateInput {
                plan_id: &plan.id,
                title: &unit_title,
                goal: None,
                idx: Some(idx as i64),
                execution_mode: Some("parallel"),
            },
        )?
        .ok_or_else(|| ApiError::internal("unit creation returned None"))?;
        created_units.push(u);
    }

    // Create a cycle anchored to the first unit.
    // PDD v3.0 A4 rollback: cross-unit tasks are allowed; the cycle's unit_id
    // is just a structural anchor — tasks in other units may carry the same cycle_id.
    let first_unit_id = &created_units[0].id;
    let cycle_title = format!("{} Round {} Cycle", body.domain, body.round);
    let cycle = cycles::create(
        &conn,
        cycles::CreateInput {
            project_id: &project_id,
            unit_id: first_unit_id,
            title: &cycle_title,
            goal: Some(&format!(
                "QA round {} for domain: {}",
                body.round, body.domain
            )),
            idx: None,
        },
    )?
    .ok_or_else(|| ApiError::internal("cycle creation returned None"))?;

    // Activate immediately so tasks can be started.
    let cycle = cycles::activate(&conn, &cycle.id)?
        .ok_or_else(|| ApiError::internal("cycle activate returned None"))?;

    app.emit(
        "discover-loop:started",
        serde_json::json!({
            "plan_id": plan.id,
            "cycle_id": cycle.id,
            "round": body.round,
            "domain": body.domain,
        }),
    );

    Ok(Json(StartResponse {
        plan,
        units: created_units,
        cycle,
    }))
}

/// DOGFOOD-005: POST /discover-loop/next-round
///
/// Complete current cycle and auto-create the next round's plan + cycle +
/// units. Domain and areas are inferred from the previous plan.
#[derive(Deserialize)]
struct NextRoundBody {
    /// Plan ID from the previous round (used to infer domain, areas, project).
    previous_plan_id: String,
    /// Override domain (optional; inferred from previous plan title if absent).
    domain: Option<String>,
    /// Override unit areas (optional; inferred from previous plan's units if absent).
    unit_areas: Option<Vec<String>>,
    /// Round number to create. If absent, inferred as prev_round + 1.
    round: Option<u32>,
}

async fn next_round(
    State(app): State<AppState>,
    Json(body): Json<NextRoundBody>,
) -> ApiResult<Json<StartResponse>> {
    // Phase 1 (sync, holds MutexGuard): read prev-plan / units / cycles, mark
    // previous cycles complete, then drop the guard before the .await on
    // start(). Wrapping the sync part in its own scope keeps the guard out of
    // the await suspension's captured set so the future stays Send.
    struct PrepResult {
        project_id: String,
        domain: String,
        round: u32,
        unit_areas: Vec<String>,
        /// Cycles this call closed, carried out so the SSE emit can happen after
        /// the guard is dropped.
        superseded_cycles: Vec<String>,
    }
    let prep: PrepResult = {
        let conn = app.conn();

        let prev_plan = plans::get(&conn, &body.previous_plan_id)?
            .ok_or_else(|| ApiError::not_found("previous plan not found"))?;

        // R2 DOGFOOD-033 fix: refuse to start a new round when the previous plan
        // is already converged (status=completed). Once converged, next-round is
        // a regression — caller must explicitly create a new fix plan instead.
        if prev_plan.status == "completed" {
            let prev_counts = query_plan_task_counts(&conn, &prev_plan.id).unwrap_or(TaskCounts {
                pass: 0,
                defect: 0,
                scenario_error: 0,
                total: 0,
            });
            if prev_counts.defect == 0 && prev_counts.scenario_error == 0 {
                return Err(ApiError::bad_request_coded(
                    "ALREADY_CONVERGED",
                    "ALREADY_CONVERGED: previous round plan is completed with defect=0 + scenario_error=0; next-round refused (qa-flow §7).",
                ));
            }
        }

        // Infer domain and round from previous plan title ("Domain Round N").
        let (inferred_domain, inferred_round) = parse_round_plan_title(&prev_plan.title)
            .unwrap_or_else(|| (prev_plan.title.clone(), 1));
        let domain = body.domain.clone().unwrap_or(inferred_domain);
        let next_round_num = body.round.unwrap_or(inferred_round + 1);

        // Infer unit areas from previous plan's units.
        let prev_units = units::list(
            &conn,
            units::ListFilter {
                plan_id: Some(&body.previous_plan_id),
            },
        )?;
        let inferred_areas: Vec<String> = prev_units
            .iter()
            .map(|u| {
                let prefix = format!("QA-{} ", domain);
                u.title
                    .strip_prefix(&prefix)
                    .unwrap_or(&u.title)
                    .to_string()
            })
            .collect();
        let unit_areas = body.unit_areas.clone().unwrap_or(inferred_areas);

        if unit_areas.is_empty() {
            return Err(ApiError::bad_request_coded(
                "MISSING_UNIT_AREAS",
                "MISSING_UNIT_AREAS: could not infer unit_areas from previous plan — supply explicitly",
            ));
        }

        // R2 DOGFOOD-004 fix: complete the previous-plan active cycles BEFORE
        // returning so Round R+1 never coexists with R's still-active cycle.
        let cycles_to_complete: Vec<String> = {
            let prev_cycles = cycles::list(
                &conn,
                cycles::ListFilter {
                    project_id: Some(&prev_plan.project_id),
                    unit_id: None,
                    status: Some("active"),
                },
            )?;
            let prev_unit_ids: std::collections::HashSet<String> =
                prev_units.iter().map(|u| u.id.clone()).collect();
            prev_cycles
                .into_iter()
                .filter(|c| {
                    c.unit_id
                        .as_ref()
                        .map(|uid| prev_unit_ids.contains(uid))
                        .unwrap_or(false)
                })
                .map(|c| c.id)
                .collect()
        };
        // `force_complete`, not `complete`: starting round R+1 supersedes round R
        // whatever R left behind, and an unresolved defect is exactly what it
        // leaves. The residue gate refuses those rows, and the only handling here
        // is a warning, so a gated close would leave the cycle `active` and let
        // R+1 run beside it — which is what this block exists to prevent.
        // Superseding a round is not the same act as declaring it finished.
        let mut superseded: Vec<String> = Vec::new();
        for cid in &cycles_to_complete {
            match cycles::force_complete(&conn, cid) {
                // `Some` only: `Ok(None)` means the row was gone by the time we
                // read it back (deleted concurrently), and announcing an update to
                // a cycle that no longer exists sends subscribers after nothing.
                // Every other `cycle:updated` site guards the same way.
                Ok(Some(_)) => superseded.push(cid.clone()),
                Ok(None) => tracing::warn!(
                    cycle_id = %cid,
                    "discover-loop:next-round — previous cycle vanished before it could be closed"
                ),
                Err(e) => tracing::warn!(
                    cycle_id = %cid,
                    error = %e,
                    "discover-loop:next-round — failed to auto-complete previous cycle (continuing)"
                ),
            }
        }

        PrepResult {
            project_id: prev_plan.project_id,
            domain,
            round: next_round_num,
            unit_areas,
            superseded_cycles: superseded,
        }
    };

    // Announce the closures. `force_complete` is a repo-layer UPDATE and the
    // `cycle:updated` emit lives in the route handlers this path bypasses, so
    // without this a subscriber keeps rendering round R as active until some
    // unrelated event arrives — the row is repaired, the wire is not.
    //
    // Payload is `{"id"}` only, matching `routes::cycles`: subscribers refetch on
    // this event, so adding fields here would create a second shape for the same
    // event name — and a client that read them would be trusting a payload the
    // other emit sites do not send.
    for cid in &prep.superseded_cycles {
        app.emit("cycle:updated", serde_json::json!({ "id": cid }));
    }

    let start_body = StartBody {
        project_id: prep.project_id,
        domain: prep.domain,
        round: prep.round,
        unit_areas: prep.unit_areas,
        description: None,
    };
    start(State(app), Json(start_body)).await
}

/// Parse "<domain> Round <N>" → (domain, N). Returns None if pattern doesn't match.
fn parse_round_plan_title(title: &str) -> Option<(String, u32)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(.+) Round (\d+)$").unwrap());
    re.captures(title).and_then(|caps| {
        let domain = caps.get(1)?.as_str().to_string();
        let round: u32 = caps.get(2)?.as_str().parse().ok()?;
        Some((domain, round))
    })
}

// ---------------------------------------------------------------------------
// B. Dispatch metadata + TSV schema validation
// ---------------------------------------------------------------------------

/// DOGFOOD-011: GET /discover-loop/dispatch-plan?plan_id=<id>&batch_size=30
///
/// Read the plan's units + scenario artifact counts and output a batch
/// dispatch manifest. Enforces X7 anti-pattern guard (batch_size ≤ 30 per
/// PDD A8). Generates batch_ids the caller can embed in TSV evidence rows.
#[derive(Deserialize)]
struct DispatchPlanQuery {
    plan_id: String,
    /// Maximum scenarios per sub-agent batch (PDD A8 hard cap: 30). Default 30.
    #[serde(default = "default_batch_size")]
    batch_size: u32,
}

fn default_batch_size() -> u32 {
    30
}

#[derive(Serialize)]
struct DispatchManifest {
    plan_id: String,
    batch_size: u32,
    /// true when any unit's scenario_count exceeds batch_size (X7 violation).
    x7_violation: bool,
    units: Vec<UnitDispatchInfo>,
}

#[derive(Serialize)]
struct UnitDispatchInfo {
    unit_id: String,
    unit_title: String,
    /// First knowledge entry attached to this unit, if any.
    scenario_knowledge_id: Option<String>,
    scenario_count: u32,
    batches_needed: u32,
    /// Pre-generated BATCH-<ULID> identifiers — one per batch.
    batch_ids: Vec<String>,
    /// R3 DOGFOOD-012 fix: per-batch scenario size (length == batches_needed).
    /// For scenario_count=35, batch_size=30 → [30, 5]. Sub-agents read this to
    /// know exactly how many scenarios to reason over per batch.
    batch_sizes: Vec<u32>,
}

async fn dispatch_plan(
    State(app): State<AppState>,
    Query(q): Query<DispatchPlanQuery>,
) -> ApiResult<Json<DispatchManifest>> {
    if q.batch_size == 0 || q.batch_size > 200 {
        return Err(ApiError::bad_request_coded(
            "INVALID_BATCH_SIZE",
            "INVALID_BATCH_SIZE: batch_size must be 1..=200 (PDD A8 recommended cap: 30)",
        ));
    }

    let conn = app.conn();
    let plan =
        plans::get(&conn, &q.plan_id)?.ok_or_else(|| ApiError::not_found("plan not found"))?;

    let plan_units = units::list(
        &conn,
        units::ListFilter {
            plan_id: Some(&plan.id),
        },
    )?;

    let mut x7_violation = false;
    let mut unit_infos: Vec<UnitDispatchInfo> = Vec::new();

    for u in &plan_units {
        // Look for a knowledge entry attached to this unit.
        let rag_knowledge = knowledge::list(
            &conn,
            knowledge::ListFilter {
                unit_id: Some(&u.id),
                ..Default::default()
            },
        )?;
        let first_rag = rag_knowledge.into_iter().next();
        let scenario_count: u32 = first_rag
            .as_ref()
            .map(|a| count_scenario_ids_in_content(&a.content))
            .unwrap_or(0);

        let batches_needed = if scenario_count == 0 {
            1
        } else {
            scenario_count.div_ceil(q.batch_size)
        };

        // R3 DOGFOOD-012 fix: x7_violation must NOT fire on auto-splittable
        // cases (e.g., 35 scenarios at batch_size=30 → batches=[30, 5] is
        // valid). It fires only when the *requested* batch_size itself
        // exceeds the PDD A8 cap (30) — that signals the caller asked for an
        // anti-pattern, not the count.
        if q.batch_size > 30 {
            x7_violation = true;
        }

        let batch_ids: Vec<String> = (0..batches_needed)
            .map(|_| format!("BATCH-{}", ulid()))
            .collect();

        // R3 DOGFOOD-012 fix: emit per-batch size manifest so sub-agents can
        // partition the scenario list deterministically (front-loaded fill
        // strategy: every batch == batch_size except the last which carries
        // the remainder).
        let mut batch_sizes: Vec<u32> = Vec::with_capacity(batches_needed as usize);
        if scenario_count == 0 {
            batch_sizes.push(0);
        } else {
            let mut remaining = scenario_count;
            for _ in 0..batches_needed {
                let take = remaining.min(q.batch_size);
                batch_sizes.push(take);
                remaining -= take;
            }
        }

        unit_infos.push(UnitDispatchInfo {
            unit_id: u.id.clone(),
            unit_title: u.title.clone(),
            scenario_knowledge_id: first_rag.map(|a| a.id),
            scenario_count,
            batches_needed,
            batch_ids,
            batch_sizes,
        });
    }

    Ok(Json(DispatchManifest {
        plan_id: q.plan_id,
        batch_size: q.batch_size,
        x7_violation,
        units: unit_infos,
    }))
}

/// DOGFOOD-015: POST /discover-loop/verify-tsv
///
/// Validate a TSV payload against the required 6-field schema (qa-flow §5):
///   scenario_id<TAB>status<TAB>reasoning<TAB>evidence<TAB>tier_used<TAB>batch_id
///
/// Returns { valid, row_count, error_count, errors }. Does NOT write to DB.
#[derive(Deserialize)]
struct VerifyTsvBody {
    /// Raw TSV content. Header row (first line without US- prefix) is skipped.
    tsv: String,
}

#[derive(Serialize)]
struct TsvValidationResult {
    valid: bool,
    row_count: u32,
    error_count: u32,
    errors: Vec<TsvRowError>,
}

#[derive(Serialize)]
struct TsvRowError {
    row: u32,
    field: String,
    message: String,
}

async fn verify_tsv(Json(body): Json<VerifyTsvBody>) -> ApiResult<Json<TsvValidationResult>> {
    Ok(Json(validate_tsv_content(&body.tsv)))
}

/// Core TSV validation logic — pure, no DB access (also called from bulk_sync).
///
/// Schema (7 fields, qa-flow §5 v3.0 — R2 DOGFOOD-013/017 fix):
///   scenario_id<TAB>status<TAB>reasoning<TAB>evidence<TAB>tier_used<TAB>batch_id<TAB>escalation_reason
///
/// Backward compat: 6-field rows are still accepted (escalation_reason
/// inferred as empty) so existing R1 TSVs do not break, but verify-tsv
/// emits a `schema_version` warning — sub-agents should emit 7 columns
/// going forward.
///
/// Required-field policy (R2 DOGFOOD-043/044 fix):
///   - scenario_id, status, reasoning: always required
///   - evidence: always required (was: only on defect/scenario_error).
///     Pass rows must cite the file:line they reasoned over so future
///     regressions can compare against the same evidence.
///   - tier_used, batch_id: required for sub-agent-emitted rows; the
///     bulk-sync caller enforces this via `--strict-schema`.
///   - escalation_reason: required when tier_used == "opus" (escalation
///     justification per qa-flow §2.5).
fn validate_tsv_content(tsv: &str) -> TsvValidationResult {
    static SCENARIO_ID_RE: OnceLock<Regex> = OnceLock::new();
    let sc_re = SCENARIO_ID_RE.get_or_init(|| Regex::new(r"^US-[A-Z][A-Z0-9-]*-\d{3,}$").unwrap());

    static BATCH_ID_RE: OnceLock<Regex> = OnceLock::new();
    let batch_re =
        BATCH_ID_RE.get_or_init(|| Regex::new(r"^BATCH-[0-9A-HJKMNP-TV-Z]{26}$").unwrap());

    let mut errors: Vec<TsvRowError> = Vec::new();
    let mut row_count: u32 = 0;

    for (line_idx, line) in tsv.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();

        // Skip header row heuristic: first field in line 0 doesn't look like a scenario ID.
        if line_idx == 0 && (fields.is_empty() || !sc_re.is_match(fields[0])) {
            continue;
        }

        row_count += 1;
        let row_num = row_count;

        // Accept 6 (legacy) or 7 (v3.0) fields. Anything else → schema error.
        if fields.len() != 6 && fields.len() != 7 {
            errors.push(TsvRowError {
                row: row_num,
                field: "all".to_string(),
                message: format!(
                    "expected 6 (legacy) or 7 (v3.0) tab-separated fields, got {}",
                    fields.len()
                ),
            });
            continue;
        }

        let scenario_id = fields[0];
        let status = fields[1];
        let reasoning = fields[2];
        let evidence = fields[3];
        let tier_used = fields[4];
        let batch_id = fields[5];
        let escalation_reason = if fields.len() == 7 { fields[6] } else { "" };

        // scenario_id validation
        if !sc_re.is_match(scenario_id) {
            errors.push(TsvRowError {
                row: row_num,
                field: "scenario_id".to_string(),
                message: format!(
                    "does not match ^US-[A-Z][A-Z0-9-]*-\\d{{3,}}$ : {:?}",
                    scenario_id
                ),
            });
        }

        // status validation
        match status {
            "pass" | "defect" | "scenario_error" => {}
            other => errors.push(TsvRowError {
                row: row_num,
                field: "status".to_string(),
                message: format!("must be pass|defect|scenario_error, got {:?}", other),
            }),
        }

        // reasoning must not be empty
        if reasoning.trim().is_empty() {
            errors.push(TsvRowError {
                row: row_num,
                field: "reasoning".to_string(),
                message: "reasoning must not be empty (X8 anti-pattern)".to_string(),
            });
        }

        // evidence: always required (R2 DOGFOOD-043 fix — pass rows must cite
        // file:line for future regression comparison).
        if evidence.trim().is_empty() {
            errors.push(TsvRowError {
                row: row_num,
                field: "evidence".to_string(),
                message: format!(
                    "evidence is required (X8 — must be file:line or reasoning summary, all status incl. {status})"
                ),
            });
        }

        // tier_used: haiku | sonnet | opus or empty
        if !tier_used.is_empty() && !matches!(tier_used, "haiku" | "sonnet" | "opus") {
            errors.push(TsvRowError {
                row: row_num,
                field: "tier_used".to_string(),
                message: format!("expected haiku|sonnet|opus or empty, got {:?}", tier_used),
            });
        }

        // batch_id: BATCH-<ULID> required (R2 DOGFOOD-044 fix — drop the
        // optional design; every sub-agent emission must declare its batch).
        if batch_id.is_empty() {
            errors.push(TsvRowError {
                row: row_num,
                field: "batch_id".to_string(),
                message:
                    "batch_id is required (R2 DOGFOOD-044 fix — expected BATCH-<26-char-ULID>)"
                        .to_string(),
            });
        } else if !batch_re.is_match(batch_id) {
            errors.push(TsvRowError {
                row: row_num,
                field: "batch_id".to_string(),
                message: format!("expected BATCH-<26-char-ULID>, got {:?}", batch_id),
            });
        }

        // escalation_reason: required when tier_used == "opus" (qa-flow §2.5
        // Tier escalation must be justified). Optional otherwise.
        if tier_used == "opus" && escalation_reason.trim().is_empty() {
            errors.push(TsvRowError {
                row: row_num,
                field: "escalation_reason".to_string(),
                message: "escalation_reason is required when tier_used=opus (qa-flow §2.5)"
                    .to_string(),
            });
        }
    }

    TsvValidationResult {
        valid: errors.is_empty(),
        row_count,
        error_count: errors.len() as u32,
        errors,
    }
}

/// DOGFOOD-020: POST /discover-loop/batch-id
///
/// Generate a fresh BATCH-<ULID> identifier for embedding in TSV evidence rows.
async fn generate_batch_id() -> ApiResult<Json<Value>> {
    let id = format!("BATCH-{}", ulid());
    Ok(Json(serde_json::json!({ "batch_id": id })))
}

// ---------------------------------------------------------------------------
// C. Bulk sync transcription: TSV → DB task status
// ---------------------------------------------------------------------------

/// DOGFOOD-021: POST /discover-loop/sync
///
/// Transcribe TSV evidence rows into Clawket tasks.
///
/// X9 anti-pattern prevention: this handler contains ZERO reasoning logic.
/// It is pure transcription: TSV `status` field → task `status` field.
///
/// Status mapping (qa-flow.md §0):
///   pass           → task status = done, qa_status = pass
///   defect         → task status = blocked, qa_status = defect
///   scenario_error → task status = cancelled, qa_status = scenario_error
///
/// Idempotent: if a task with `scenario_id` already exists in the cycle, it
/// is updated in-place rather than duplicated.
#[derive(Deserialize)]
struct BulkSyncBody {
    /// Raw TSV (same 6-field schema as verify-tsv).
    tsv: String,
    /// Target unit ID for new tasks.
    unit_id: String,
    /// Target cycle ID (must be active).
    cycle_id: String,
    /// Assignee label for created tasks (default: "discover-loop").
    assignee: Option<String>,
}

#[derive(Serialize)]
struct BulkSyncResult {
    synced: u32,
    skipped: u32,
    errors: Vec<SyncRowError>,
}

#[derive(Serialize)]
struct SyncRowError {
    row: u32,
    scenario_id: String,
    message: String,
}

async fn bulk_sync(
    State(app): State<AppState>,
    Json(body): Json<BulkSyncBody>,
) -> ApiResult<Json<BulkSyncResult>> {
    // R3 DOGFOOD-028/029 fix: auto-set CLAWKET_SYNC_CONTEXT inside the daemon
    // process for the duration of the sync so any tracing hook / spawned
    // subprocess that inherits env sees the marker, and unset it on exit.
    // This is a process-local marker — the agent that invoked the CLI also
    // sets it client-side. Both ends covered for layered hook visibility.
    struct SyncContextGuard {
        prev: Option<String>,
    }
    impl Drop for SyncContextGuard {
        fn drop(&mut self) {
            // SAFETY: single-threaded teardown of this guard's own writes.
            unsafe {
                match self.prev.take() {
                    Some(p) => std::env::set_var("CLAWKET_SYNC_CONTEXT", p),
                    None => std::env::remove_var("CLAWKET_SYNC_CONTEXT"),
                }
            }
        }
    }
    let _sync_ctx_guard = {
        let prev = std::env::var("CLAWKET_SYNC_CONTEXT").ok();
        // SAFETY: env writes in axum handlers occur on the request-handling
        // task; readers in this process tree (tracing hooks) tolerate the
        // brief unsynchronised window. The guard restores on drop.
        unsafe {
            std::env::set_var("CLAWKET_SYNC_CONTEXT", "bulk-sync");
        }
        SyncContextGuard { prev }
    };

    // Step 1: Validate TSV schema (X8 guard) before touching DB.
    let validation = validate_tsv_content(&body.tsv);
    if !validation.valid {
        return Err(ApiError::bad_request_with_details(
            format!(
                "TSV validation failed ({} error(s)) — refusing sync (X8/X9 guard)",
                validation.error_count
            ),
            serde_json::json!({ "tsv_errors": validation.errors }),
        ));
    }
    if validation.row_count == 0 {
        return Ok(Json(BulkSyncResult {
            synced: 0,
            skipped: 0,
            errors: vec![],
        }));
    }

    let assignee = body
        .assignee
        .as_deref()
        .unwrap_or("discover-loop")
        .to_string();

    let mut conn = app.conn();
    let mut synced: u32 = 0;
    let mut skipped: u32 = 0;
    let mut sync_errors: Vec<SyncRowError> = Vec::new();

    static SCENARIO_ID_RE: OnceLock<Regex> = OnceLock::new();
    let sc_re = SCENARIO_ID_RE.get_or_init(|| Regex::new(r"^US-[A-Z][A-Z0-9-]*-\d{3,}$").unwrap());

    let mut data_row: u32 = 0;
    let mut failed_scenario_ids: Vec<String> = Vec::new();
    // R3 DOGFOOD-020 fix: per-batch row-order tracking for front/back-half
    // attention-dilution stats. Each entry: (batch_id, ordinal, status).
    // Computed into a stats artifact at end-of-sync.
    let mut batch_row_log: Vec<(String, String)> = Vec::new();
    for line in body.tsv.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        // Skip header row.
        if data_row == 0 && (fields.is_empty() || !sc_re.is_match(fields[0])) {
            continue;
        }
        data_row += 1;
        // Accept 6 (legacy) or 7 (v3.0) fields — validation already gated above.
        if fields.len() != 6 && fields.len() != 7 {
            continue;
        }

        let scenario_id = fields[0];
        let status = fields[1];
        let reasoning = fields[2];
        let evidence = fields[3];
        let tier_used = fields[4];
        let batch_id = fields[5];
        // R2 DOGFOOD-016/017 fix: extract escalation_reason from 7th column;
        // also map scenario_error rows' reasoning into scenario_amendment so
        // /scenario-refine can read the agent's atomic-decomposition proposal.
        let escalation_reason_field = if fields.len() == 7 { fields[6] } else { "" };

        // X9: pure status transcription — NO reasoning decisions here.
        let task_status = match status {
            "pass" => "done",
            "defect" => "blocked",
            "scenario_error" => "cancelled",
            _ => {
                sync_errors.push(SyncRowError {
                    row: data_row,
                    scenario_id: scenario_id.to_string(),
                    message: format!("unknown status {:?}", status),
                });
                skipped += 1;
                continue;
            }
        };

        // R3 DOGFOOD-020 fix: log row's batch + status in arrival order so we
        // can split each batch into front/back halves for attention-dilution
        // stats. We log all valid-status rows (pass/defect/scenario_error).
        if !batch_id.is_empty() {
            batch_row_log.push((batch_id.to_string(), status.to_string()));
        }

        let qa_status_val: Option<&str> = match status {
            "pass" | "defect" | "scenario_error" => Some(status),
            _ => None,
        };

        let evidence_opt = if evidence.trim().is_empty() {
            None
        } else {
            Some(evidence)
        };
        let tier_opt = if tier_used.is_empty() {
            None
        } else {
            Some(tier_used)
        };
        let batch_opt = if batch_id.is_empty() {
            None
        } else {
            Some(batch_id)
        };
        let escalation_opt = if escalation_reason_field.trim().is_empty() {
            None
        } else {
            Some(escalation_reason_field)
        };
        // R2 DOGFOOD-016 fix: scenario_error rows carry the agent's amendment
        // proposal in the `reasoning` field. Mirror it into scenario_amendment
        // so /scenario-refine has structured access without re-parsing body.
        let scenario_amendment_opt: Option<&str> = if status == "scenario_error" {
            Some(reasoning)
        } else {
            None
        };

        let task_title = format!("QA-{}", scenario_id);

        // Idempotency: check for existing task with this scenario_id in this cycle.
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM tasks WHERE scenario_id = ?1 AND cycle_id = ?2 LIMIT 1",
                rusqlite::params![scenario_id, body.cycle_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap_or(None);

        if let Some(ref eid) = existing_id {
            let uf = tasks::UpdateFields {
                status: Some(task_status.to_string()),
                qa_status: Some(qa_status_val.map(|s| s.to_string())),
                evidence: Some(evidence_opt.map(|s| s.to_string())),
                tier_used: Some(tier_opt.map(|s| s.to_string())),
                batch_id: Some(batch_opt.map(|s| s.to_string())),
                escalation_reason: Some(escalation_opt.map(|s| s.to_string())),
                scenario_amendment: Some(scenario_amendment_opt.map(|s| s.to_string())),
                body: Some(Some(reasoning.to_string())),
                ..Default::default()
            };

            match tasks::update(&mut conn, eid, uf) {
                Ok((_, _cascade)) => {
                    app.emit(
                        "task:updated",
                        serde_json::json!({ "id": eid, "source": "discover-loop:sync" }),
                    );
                    synced += 1;
                }
                Err(e) => {
                    sync_errors.push(SyncRowError {
                        row: data_row,
                        scenario_id: scenario_id.to_string(),
                        message: format!("update failed: {e}"),
                    });
                    failed_scenario_ids.push(scenario_id.to_string());
                    skipped += 1;
                }
            }
        } else {
            // Create new task then immediately transition to the transcribed status.
            match tasks::create(
                &mut conn,
                tasks::CreateInput {
                    unit_id: &body.unit_id,
                    title: &task_title,
                    body: Some(reasoning),
                    assignee: Some(&assignee),
                    idx: None,
                    depends_on: vec![],
                    parent_task_id: None,
                    priority: Some("medium"),
                    complexity: None,
                    estimated_edits: None,
                    cycle_id: Some(&body.cycle_id),
                    reporter: Some("discover-loop"),
                    type_: Some("test"),
                    atomic_size_hint: None,
                    decomposition_policy: None,
                    tier: tier_opt,
                    qa_status: qa_status_val,
                    scenario_id: Some(scenario_id),
                    defect_task: None,
                    // R2 DOGFOOD-016 fix: persist scenario_error amendment text.
                    scenario_amendment: scenario_amendment_opt,
                    evidence: evidence_opt,
                    batch_id: batch_opt,
                },
            ) {
                Ok(Some(task)) => {
                    let sf = tasks::UpdateFields {
                        status: Some(task_status.to_string()),
                        escalation_reason: escalation_opt.map(|er| Some(er.to_string())),
                        ..Default::default()
                    };
                    if let Err(e) = tasks::update(&mut conn, &task.id, sf) {
                        sync_errors.push(SyncRowError {
                            row: data_row,
                            scenario_id: scenario_id.to_string(),
                            message: format!("status transition failed: {e}"),
                        });
                        failed_scenario_ids.push(scenario_id.to_string());
                        skipped += 1;
                        continue;
                    }
                    app.emit(
                        "task:created",
                        serde_json::json!({ "id": task.id, "source": "discover-loop:sync" }),
                    );
                    synced += 1;
                }
                Ok(None) => {
                    sync_errors.push(SyncRowError {
                        row: data_row,
                        scenario_id: scenario_id.to_string(),
                        message: "task create returned None".to_string(),
                    });
                    failed_scenario_ids.push(scenario_id.to_string());
                    skipped += 1;
                }
                Err(e) => {
                    sync_errors.push(SyncRowError {
                        row: data_row,
                        scenario_id: scenario_id.to_string(),
                        message: format!("create failed: {e}"),
                    });
                    failed_scenario_ids.push(scenario_id.to_string());
                    skipped += 1;
                }
            }
        }
    }

    // R2 DOGFOOD-025 fix: persist a retry-queue artifact when any rows failed
    // so a follow-up sync can resume by scenario_id without re-reasoning.
    if !failed_scenario_ids.is_empty() {
        let retry_title = format!(
            "discover-loop retry queue — cycle {} ({} rows)",
            body.cycle_id,
            failed_scenario_ids.len()
        );
        let retry_body = serde_json::json!({
            "cycle_id": body.cycle_id,
            "unit_id": body.unit_id,
            "failed_scenario_ids": failed_scenario_ids,
            "errors": sync_errors,
        })
        .to_string();
        let _ = knowledge::create(
            &conn,
            knowledge::CreateInput {
                title: &retry_title,
                content: Some(&retry_body),
                content_format: Some("json"),
                type_: "retry_queue",
                plan_id: None,
                unit_id: Some(&body.unit_id),
                task_id: None,
                parent_id: None,
            },
        );
    }

    // R2 DOGFOOD-018/019 fix + R3 DOGFOOD-018 follow-up: persist the synced TSV
    // itself as a Round R evidence artifact so batch_id distinct sets can be
    // reconciled against the DB tasks. Title format MUST follow qa-flow §5
    // verbatim: "Round R evidence — <도메인>" (round and domain inferred from
    // the cycle's plan title).
    if validation.row_count > 0 {
        let (round_label, domain_label) = match cycles::get(&conn, &body.cycle_id)
            .ok()
            .flatten()
            .and_then(|cyc| {
                cyc.unit_id
                    .and_then(|uid| units::get(&conn, &uid).ok().flatten())
                    .and_then(|u| plans::get(&conn, &u.plan_id).ok().flatten())
            })
            .and_then(|plan| parse_round_plan_title(&plan.title))
        {
            Some((domain, round)) => (format!("Round {round}"), domain),
            None => ("Round ?".to_string(), "unknown".to_string()),
        };
        let evidence_title = format!(
            "{} evidence — {} ({} rows synced)",
            round_label, domain_label, synced
        );
        let _ = knowledge::create(
            &conn,
            knowledge::CreateInput {
                title: &evidence_title,
                content: Some(&body.tsv),
                content_format: Some("tsv"),
                type_: "evidence",
                plan_id: None,
                unit_id: Some(&body.unit_id),
                task_id: None,
                parent_id: None,
            },
        );

        // R3 DOGFOOD-020 fix: front/back-half attention-dilution stats
        // artifact. For each batch_id, partition rows by arrival order into
        // front-half (first floor(N/2)) and back-half (rest). Compute defect
        // ratio per half. If back-half defect ratio exceeds front-half by
        // ≥ 1.5× we record an `attention_dilution_suspected=true` flag —
        // the agent skill (PDD A8) decides whether to re-dispatch the batch.
        let mut by_batch: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (bid, st) in &batch_row_log {
            by_batch.entry(bid.clone()).or_default().push(st.clone());
        }
        if !by_batch.is_empty() {
            let mut per_batch_stats: Vec<serde_json::Value> = Vec::new();
            for (bid, statuses) in &by_batch {
                let n = statuses.len();
                let mid = n / 2; // front: [0..mid), back: [mid..n)
                let front = &statuses[..mid];
                let back = &statuses[mid..];
                let count = |slice: &[String], pat: &str| -> u32 {
                    slice.iter().filter(|s| s.as_str() == pat).count() as u32
                };
                let f_total = front.len() as u32;
                let b_total = back.len() as u32;
                let f_defect = count(front, "defect");
                let b_defect = count(back, "defect");
                let f_pass = count(front, "pass");
                let b_pass = count(back, "pass");
                let f_se = count(front, "scenario_error");
                let b_se = count(back, "scenario_error");
                let f_ratio = if f_total > 0 {
                    f_defect as f64 / f_total as f64
                } else {
                    0.0
                };
                let b_ratio = if b_total > 0 {
                    b_defect as f64 / b_total as f64
                } else {
                    0.0
                };
                // Threshold per PDD A8 / qa-flow §8: back ≥ 1.5× front (and
                // absolute back-defect ≥ 2 to avoid noise on tiny batches).
                let attention_dilution_suspected = b_defect >= 2 && b_ratio >= f_ratio * 1.5;
                per_batch_stats.push(serde_json::json!({
                    "batch_id": bid,
                    "row_count": n,
                    "front_half": {
                        "rows": f_total, "pass": f_pass, "defect": f_defect,
                        "scenario_error": f_se, "defect_ratio": f_ratio,
                    },
                    "back_half": {
                        "rows": b_total, "pass": b_pass, "defect": b_defect,
                        "scenario_error": b_se, "defect_ratio": b_ratio,
                    },
                    "attention_dilution_suspected": attention_dilution_suspected,
                    "ratio_back_over_front": if f_ratio > 0.0 { b_ratio / f_ratio } else { 0.0 },
                }));
            }
            let stats_title = format!(
                "{} attention-dilution stats — {} ({} batches)",
                round_label,
                domain_label,
                by_batch.len()
            );
            let stats_body = serde_json::json!({
                "cycle_id": body.cycle_id,
                "unit_id": body.unit_id,
                "round": round_label,
                "domain": domain_label,
                "batches": per_batch_stats,
            })
            .to_string();
            let _ = knowledge::create(
                &conn,
                knowledge::CreateInput {
                    title: &stats_title,
                    content: Some(&stats_body),
                    content_format: Some("json"),
                    type_: "attention_dilution_stats",
                    plan_id: None,
                    unit_id: Some(&body.unit_id),
                    task_id: None,
                    parent_id: None,
                },
            );
        }
    }

    Ok(Json(BulkSyncResult {
        synced,
        skipped,
        errors: sync_errors,
    }))
}

// ---------------------------------------------------------------------------
// D. 3-way convergence query + next-round determination
// ---------------------------------------------------------------------------

/// DOGFOOD-031: GET /discover-loop/status?plan_id=<id>  (or ?project_id=<id>)
///
/// Returns defect/scenario_error/pass counts for the active round plan, plus
/// a monotone-decrease comparison vs the previous round (regression detection).
#[derive(Deserialize)]
struct ConvergenceQuery {
    plan_id: Option<String>,
    project_id: Option<String>,
}

#[derive(Serialize)]
struct ConvergenceStatusResponse {
    plan_id: String,
    plan_title: String,
    round: Option<u32>,
    counts: TaskCounts,
    /// true when defect=0 AND scenario_error=0 for this round.
    converged: bool,
    /// true when defect count is > 0 and higher than the previous round.
    regression_detected: bool,
    previous_round: Option<PreviousRoundSummary>,
}

#[derive(Serialize)]
pub(crate) struct TaskCounts {
    pub(crate) pass: i64,
    pub(crate) defect: i64,
    pub(crate) scenario_error: i64,
    pub(crate) total: i64,
}

#[derive(Serialize)]
struct PreviousRoundSummary {
    plan_id: String,
    plan_title: String,
    counts: TaskCounts,
}

async fn convergence_status(
    State(app): State<AppState>,
    Query(q): Query<ConvergenceQuery>,
) -> ApiResult<Json<ConvergenceStatusResponse>> {
    let conn = app.conn();

    let plan = resolve_plan(&conn, q.plan_id.as_deref(), q.project_id.as_deref())?;
    let round = parse_round_plan_title(&plan.title).map(|(_, r)| r);
    let counts = query_plan_task_counts(&conn, &plan.id)?;

    // Look up previous round plan for monotone-decrease check.
    let prev_summary = if let Some(r) = round {
        if r > 1 {
            find_prev_round_plan(&conn, &plan.project_id, &plan.title, r - 1)?
        } else {
            None
        }
    } else {
        None
    };

    let converged = counts.defect == 0 && counts.scenario_error == 0;

    // The baseline is the count round R-1 REPORTED at the time, read from the audit
    // log, not a re-tally of R-1's rows now.
    //
    // Re-tallying makes the comparison unstable in the ordinary case: fixing a
    // defect is `blocked → done` with no verdict on the patch, which drops that row
    // from R-1's count. R1 finds 5, R2 has 2 left, R1 re-tallies as 0 — and R2 is
    // written to a durable audit entry as `regression` when defects strictly
    // decreased. Cancellation has the same effect, and the previous reasoning
    // ("a withdrawn defect was never a real one") was at least arguable there; it
    // does not survive being extended to a defect that was fixed.
    //
    // The audit log already records what each round reported, so the snapshot needs
    // no new storage. Falling back to the live tally keeps rounds that predate the
    // audit entry working — those can still drift, which is strictly better than
    // refusing to compare.
    let logged_prev_defect = round
        .and_then(|r| r.checked_sub(1))
        .and_then(|prev| logged_round_defects(&conn, &plan.project_id, &plan.title, prev));
    let baseline_defect =
        logged_prev_defect.or_else(|| prev_summary.as_ref().map(|p| p.counts.defect));
    let regression_detected = baseline_defect
        .map(|base| counts.defect > 0 && counts.defect > base)
        .unwrap_or(false);

    // R3 DOGFOOD-035 fix: append a per-round decision line to a domain-scoped
    // audit knowledge entry ("convergence audit log — <도메인>"). The entry
    // accumulates: "R<n>: defect=X, scenario_error=Y, decision=...". Decision
    // is one of: converged | regression | continue.
    let domain = parse_round_plan_title(&plan.title)
        .map(|(d, _)| d)
        .unwrap_or_else(|| "unknown".to_string());
    let decision = if regression_detected {
        "regression"
    } else if converged {
        "converged"
    } else {
        "continue"
    };
    let line = format_audit_line(round.unwrap_or(0), &counts, decision);
    let audit_title = audit_log_title(&plan.project_id, &domain);
    let existing_audit = find_audit_entry(&conn, &plan.project_id, &domain);
    if let Some(a) = existing_audit {
        // Append (idempotent on identical line — only append if last differs).
        let prev_body = a.content.clone();
        let already_logged = prev_body
            .lines()
            .next_back()
            .map(|last| last.trim() == line)
            .unwrap_or(false);
        if !already_logged {
            let new_body = if prev_body.is_empty() {
                line.clone()
            } else {
                format!("{prev_body}\n{line}")
            };
            let _ = knowledge::update(
                &conn,
                &a.id,
                knowledge::UpdateFields {
                    content: Some(new_body),
                    ..Default::default()
                },
            );
        }
    } else {
        // `plan_id` is required, not optional: `knowledge::create` rejects a row
        // attached to nothing (`KNOWLEDGE_NEEDS_ATTACHMENT`), so passing `None` here
        // meant the entry was NEVER created — and `let _ =` swallowed the error, so
        // the audit log silently did not exist. The snapshot baseline built on top of
        // it could therefore never find anything and always fell back to the live
        // re-tally it was meant to replace.
        //
        // The entry spans every round of a domain, so no single plan owns it; the
        // round that first creates it supplies the attachment, and the project
        // scoping lives in the title. A failure is logged rather than dropped —
        // silently losing the log is what hid this.
        if let Err(e) = knowledge::create(
            &conn,
            knowledge::CreateInput {
                title: &audit_title,
                content: Some(&line),
                content_format: Some("text"),
                type_: "convergence_audit",
                plan_id: Some(&plan.id),
                unit_id: None,
                task_id: None,
                parent_id: None,
            },
        ) {
            tracing::warn!(
                plan_id = %plan.id,
                audit_title = %audit_title,
                error = %e,
                "discover-loop:status — could not create the convergence audit entry; \
                 regression detection will fall back to a live re-tally"
            );
        }
    }

    Ok(Json(ConvergenceStatusResponse {
        plan_id: plan.id,
        plan_title: plan.title,
        round,
        counts,
        converged,
        regression_detected,
        previous_round: prev_summary,
    }))
}

/// DOGFOOD-038: GET /discover-loop/converged?project_id=<id>  (or ?plan_id=<id>)
///
/// Implements qa-flow §7 last-2-rounds-zero rule:
///   converged = defect=0 + scenario_error=0 in both current AND previous round.
///
/// HTTP 200 always; converged=true means pipeline can proceed to manual QA.
#[derive(Deserialize)]
struct ConvergedQuery {
    project_id: Option<String>,
    plan_id: Option<String>,
}

#[derive(Serialize)]
struct ConvergedResponse {
    converged: bool,
    reason: String,
    current_round: Option<u32>,
    previous_round: Option<u32>,
}

async fn converged(
    State(app): State<AppState>,
    Query(q): Query<ConvergedQuery>,
) -> ApiResult<Json<ConvergedResponse>> {
    let conn = app.conn();
    let plan = resolve_plan(&conn, q.plan_id.as_deref(), q.project_id.as_deref())?;
    let current_round = parse_round_plan_title(&plan.title).map(|(_, r)| r);
    let cur_counts = query_plan_task_counts(&conn, &plan.id)?;
    let cur_zero = cur_counts.defect == 0 && cur_counts.scenario_error == 0;

    // last-2-rounds-zero check.
    let prev_zero = match current_round {
        None | Some(0) | Some(1) => false,
        Some(r) => find_prev_round_plan(&conn, &plan.project_id, &plan.title, r - 1)?
            .map(|p| p.counts.defect == 0 && p.counts.scenario_error == 0)
            .unwrap_or(false),
    };

    let c = cur_zero && prev_zero;
    let reason = if c {
        "defect=0 + scenario_error=0 for 2 consecutive rounds (qa-flow §7)".to_string()
    } else if !cur_zero {
        format!(
            "current round has defect={} scenario_error={}",
            cur_counts.defect, cur_counts.scenario_error
        )
    } else if current_round.map(|r| r <= 1).unwrap_or(true) {
        "only round 1 completed — need 2 consecutive zero rounds (qa-flow §7)".to_string()
    } else {
        "previous round was not zero — need 2 consecutive rounds".to_string()
    };

    // R3 DOGFOOD-038 fix: on convergence, append a "next: activate <도메인>
    // 수동 QA plan" line to the plan body (qa-flow §7 last-step). The plan
    // body should record what the next phase is so an outside reviewer (or
    // a later session) can pick up the QA hand-off without re-deriving it.
    if c {
        let domain = parse_round_plan_title(&plan.title)
            .map(|(d, _)| d)
            .unwrap_or_else(|| "unknown".to_string());
        let next_line = format!("next: activate {domain} 수동 QA plan");
        let prev_desc = plan.description.clone().unwrap_or_default();
        if !prev_desc.contains(&next_line) {
            let new_desc = if prev_desc.is_empty() {
                next_line.clone()
            } else {
                format!("{prev_desc}\n\n## 수렴\n\n{next_line}")
            };
            let _ = plans::update(
                &conn,
                &plan.id,
                plans::UpdateFields {
                    description: Some(Some(new_desc)),
                    ..Default::default()
                },
            );
        }
    }

    Ok(Json(ConvergedResponse {
        converged: c,
        reason,
        current_round,
        previous_round: current_round
            .map(|r| r.saturating_sub(1))
            .filter(|&r| r > 0),
    }))
}

/// DOGFOOD-033: GET /discover-loop/rounds/:project_id
///
/// List all "Round N" plans for a project in round-number order, each with
/// task counts. Powers the monotone-decrease graph in discover-loop status.
async fn list_round_plans(
    State(app): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let conn = app.conn();
    let project_id = resolve_project_ref(&conn, &project_id)?;
    let all_plans = plans::list(
        &conn,
        plans::ListFilter {
            project_id: Some(&project_id),
            status: None,
        },
    )?;

    let mut rounds: Vec<Value> = all_plans
        .into_iter()
        .filter_map(|p| {
            parse_round_plan_title(&p.title).map(|(domain, round)| {
                let counts = query_plan_task_counts(&conn, &p.id).unwrap_or(TaskCounts {
                    pass: 0,
                    defect: 0,
                    scenario_error: 0,
                    total: 0,
                });
                serde_json::json!({
                    "plan_id": p.id,
                    "plan_title": p.title,
                    "domain": domain,
                    "round": round,
                    "status": p.status,
                    "counts": counts,
                })
            })
        })
        .collect();

    // Sort ascending by round number.
    rounds.sort_by_key(|v| v.get("round").and_then(Value::as_u64).unwrap_or(0));

    Ok(Json(serde_json::json!({ "rounds": rounds })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the target plan from either plan_id or project_id (active plan fallback).
fn resolve_plan(
    conn: &rusqlite::Connection,
    plan_id: Option<&str>,
    project_id: Option<&str>,
) -> ApiResult<Plan> {
    if let Some(pid) = plan_id {
        plans::get(conn, pid)?.ok_or_else(|| ApiError::not_found("plan not found"))
    } else if let Some(proj_ref) = project_id {
        let proj_id = resolve_project_ref(conn, proj_ref)?;
        let active = plans::list(
            conn,
            plans::ListFilter {
                project_id: Some(&proj_id),
                status: Some("active"),
            },
        )?;
        active
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::not_found("no active plan for project"))
    } else {
        Err(ApiError::bad_request_coded(
            "MISSING_PARAM",
            "MISSING_PARAM: supply plan_id or project_id",
        ))
    }
}

/// SQL aggregate of task counts for a plan's **scheduled** tasks (via units join).
///
/// R2 DOGFOOD-039 fix: count by EITHER qa_status (round-result column written
/// by bulk_sync) OR task.status (workflow column also written by bulk_sync).
/// The two are kept in lock-step by `bulk_sync`, but the spec says
/// "defect 수 = blocked status task 수" so we tally a row as defect when
/// either signal indicates defect — this catches drift from manual edits
/// (e.g. an agent that flips status=blocked without setting qa_status).
///
/// LM-11093: backlog tasks (`cycle_id IS NULL`) are excluded. A task detached
/// via `task update --cycle ""` is deferred to a later round, so counting it
/// against *this* round's numbers describes work nobody scheduled. Concretely:
/// a deferred `blocked` task tallied as `defect`, which held `converged` false in
/// `convergence_status` — and since `next_round`'s ALREADY_CONVERGED guard refuses
/// the next round only when `defect == 0 && scenario_error == 0`, that phantom
/// defect also kept the guard from firing. A deferred `todo` task never reached
/// either arm, but still inflated `total`.
///
/// Note `blocked` stays a defect signal here: unlike the plan-completion gates
/// (`repo::tasks::container_terminal`), a QA round asks "did the scenarios
/// pass", and a blocked scenario did not pass. Same status, different question
/// — the exclusion that carries across is deferral, not blockedness. This is
/// one reader of "what belongs to this plan"; they are enumerated in one place,
/// at `routes::plans::counts`.
///
/// `cancelled` is the one place this reader must agree with those gates, and the
/// reason is not symmetry but termination: a cancelled row is finished work, so
/// the gates close the plan on it. `qa_status` is not cleared by a status change,
/// so abandoning a defect leaves `cancelled` + `defect` behind — and if that
/// counted as an unresolved signal, `converged` would stay false for a plan that
/// is already `completed` and cannot reopen (`PLAN_COMPLETED` freezes `create`,
/// and the only edge out of `cancelled` leads back to `todo`).
///
/// So a cancelled row counts toward `total` and **neither** unresolved bucket.
/// Moving it between them would achieve nothing: every convergence predicate ANDs
/// the two (`defect == 0 && scenario_error == 0` — in `convergence_status`,
/// `converged`, `next_round`'s ALREADY_CONVERGED guard, and rendered in
/// `routes::dashboard`), so a row in either one holds the round open exactly the
/// same. `scenario_error` means "the scenario itself was wrong and needs
/// rewriting" — a live signal `/scenario-refine` acts on — which is not what an
/// abandoned row is saying.
///
/// The three buckets are mutually exclusive but never were exhaustive — `todo`
/// and `in_progress` rows with no verdict score in none of them, which is correct
/// (a scenario that has not run yet has no result). So `pass + defect +
/// scenario_error < total` is the normal case, not a symptom; `total` is
/// `COUNT(t.id)` and no consumer treats the three as a partition. An abandoned
/// defect joins that unscored set.
pub(crate) fn query_plan_task_counts(
    conn: &rusqlite::Connection,
    plan_id: &str,
) -> anyhow::Result<TaskCounts> {
    let (pass, defect, scenario_error, total): (i64, i64, i64, i64) = conn
        .query_row(
            // A `defect` verdict counts UNLESS the row has since reached a terminal
            // status. `qa_status` is not cleared by a status change, so a verdict
            // outlives the state it was written for: `blocked → done` and
            // `blocked → cancelled` are both legal, and either leaves `defect` on a
            // row the cascade treats as finished — plan `completed`, `converged`
            // false forever, nothing an operator can clear. Excluding only
            // `cancelled` fixed one of those axes and left the other, which is how
            // this arm and the cascade drifted apart to begin with; the condition is
            // "still open", not a list of statuses.
            //
            // It is NOT `status = 'blocked'`: a defect verdict on a `todo` or
            // `in_progress` row is a real unresolved defect (DOGFOOD-039 pins
            // exactly that — a verdict written without the matching status), and the
            // cascade agrees, since neither status is container-terminal.
            //
            // The `qa_status IS NULL AND status = …` arms stay: they are
            // DOGFOOD-039's drift catch for rows whose verdict column was never
            // written, and they key on status precisely because there is no verdict
            // to consult.
            "SELECT
                SUM(CASE WHEN t.qa_status = 'pass' OR (t.qa_status IS NULL AND t.status = 'done')
                         THEN 1 ELSE 0 END),
                SUM(CASE WHEN (t.qa_status = 'defect'
                               AND t.status NOT IN ('done', 'cancelled'))
                            OR (t.qa_status IS NULL AND t.status = 'blocked')
                         THEN 1 ELSE 0 END),
                SUM(CASE WHEN t.qa_status = 'scenario_error'
                            OR (t.qa_status IS NULL AND t.status = 'cancelled')
                         THEN 1 ELSE 0 END),
                COUNT(t.id)
             FROM tasks t
             JOIN units u ON t.unit_id = u.id
             WHERE u.plan_id = ?1 AND t.cycle_id IS NOT NULL",
            rusqlite::params![plan_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap_or((0, 0, 0, 0));
    Ok(TaskCounts {
        pass,
        defect,
        scenario_error,
        total,
    })
}

/// Title of the per-domain convergence audit entry.
///
/// One function so the writer (`convergence_status`) and the reader
/// (`logged_round_defects`) cannot drift: the reader finds the entry by exact
/// title, so a change here that reached only one of them would silently degrade the
/// snapshot back to a live re-tally.
///
/// Scoped by project because two projects can share a domain name, and the baseline
/// this feeds must not cross that boundary. Knowledge rows carry `plan_id` only, and
/// this entry spans every round of a domain, so no single plan can own it — the
/// project id goes in the title instead.
fn audit_log_title(project_id: &str, domain: &str) -> String {
    format!("convergence audit log — {project_id} — {domain}")
}

/// The audit entry for this project+domain.
///
/// Exact title match, deliberately — rows written under the pre-scoping title are
/// rewritten by migration 028 rather than papered over with a fallback here. A code
/// fallback would make the scoping cosmetic: the reader would keep accepting an
/// entry that may hold another project's rounds, which is the collision the scoping
/// removes. `schema-migration-discipline.md` puts a data rename in a migration, and
/// 026 set the precedent for exactly this shape.
fn find_audit_entry(
    conn: &rusqlite::Connection,
    project_id: &str,
    domain: &str,
) -> Option<crate::models::Knowledge> {
    let title = audit_log_title(project_id, domain);
    knowledge::list(
        conn,
        knowledge::ListFilter {
            type_: Some("convergence_audit"),
            ..Default::default()
        },
    )
    .ok()?
    .into_iter()
    .find(|e| e.title == title)
}

/// One round's line in that entry. Paired with `parse_logged_round_defects`, which
/// reads it back; the round-trip is pinned by tests so a reformat here cannot
/// silently stop the reader from finding `defect=`.
fn format_audit_line(round: u32, counts: &TaskCounts, decision: &str) -> String {
    format!(
        "R{round}: defect={defect}, scenario_error={se}, pass={pass}, decision={decision}",
        defect = counts.defect,
        se = counts.scenario_error,
        pass = counts.pass,
    )
}

/// The defect count round `n` reported when it ran, read back from the audit log
/// that `convergence_status` appends to (`R<n>: defect=X, …`).
///
/// This exists so regression detection compares against what was reported rather
/// than a re-tally of that round's rows, which moves whenever a defect is fixed or
/// withdrawn. The log line is the only per-round snapshot in the system; parsing it
/// avoids adding a second store that could disagree with it.
///
/// `None` when the entry, the round's line, or the field is absent — the caller
/// falls back to the live tally rather than refusing to compare.
///
/// Scoped by project as well as domain. The baseline this replaced came from
/// `find_prev_round_plan`, which filters on `project_id`; the audit entry is keyed
/// by title alone, so two projects using the same domain name would have read each
/// other's counts into a durable `regression` verdict.
pub(crate) fn logged_round_defects(
    conn: &rusqlite::Connection,
    project_id: &str,
    current_title: &str,
    round: u32,
) -> Option<i64> {
    let domain = parse_round_plan_title(current_title).map(|(d, _)| d)?;
    let entry = find_audit_entry(conn, project_id, &domain)?;
    parse_logged_round_defects(&entry.content, round)
}

/// Pull `defect=N` for round `n` out of an audit-log body. Split from the DB lookup
/// so the parsing — which is where a format drift would bite — is testable.
///
/// Last matching line wins: a re-run appends a fresh line, and the newest is what
/// that round last reported.
fn parse_logged_round_defects(body: &str, round: u32) -> Option<i64> {
    let prefix = format!("R{round}:");
    body.lines()
        .filter_map(|l| {
            // The round label and the first field share a segment
            // (`R1: defect=5`), so strip the label before splitting on commas —
            // otherwise the first field never matches `defect=`.
            let rest = l.trim_start().strip_prefix(&prefix)?;
            rest.split(',')
                .map(str::trim)
                .find_map(|f| f.strip_prefix("defect="))
                .and_then(|v| v.trim().parse::<i64>().ok())
        })
        .next_back()
}

/// Find the previous round's plan (round = prev_round_num) in the same project.
fn find_prev_round_plan(
    conn: &rusqlite::Connection,
    project_id: &str,
    current_title: &str,
    prev_round_num: u32,
) -> ApiResult<Option<PreviousRoundSummary>> {
    let domain = parse_round_plan_title(current_title).map(|(d, _)| d);
    let Some(domain) = domain else {
        return Ok(None);
    };
    let prev_title = format!("{} Round {}", domain, prev_round_num);

    let all = plans::list(
        conn,
        plans::ListFilter {
            project_id: Some(project_id),
            status: None,
        },
    )?;

    for p in all {
        if p.title == prev_title {
            let counts = query_plan_task_counts(conn, &p.id)?;
            return Ok(Some(PreviousRoundSummary {
                plan_id: p.id,
                plan_title: p.title,
                counts,
            }));
        }
    }
    Ok(None)
}

/// Heuristic: count lines in artifact content that contain a scenario ID pattern.
fn count_scenario_ids_in_content(content: &str) -> u32 {
    static SC_LINE_RE: OnceLock<Regex> = OnceLock::new();
    let re = SC_LINE_RE.get_or_init(|| Regex::new(r"US-[A-Z][A-Z0-9-]*-\d{3,}").unwrap());
    content.lines().filter(|line| re.is_match(line)).count() as u32
}

#[cfg(test)]
mod tests {
    use super::{audit_log_title, format_audit_line, parse_logged_round_defects, TaskCounts};

    /// The writer and the reader are two halves of one format. Pinning only the
    /// parser (which is what the previous round did) leaves a reformat of
    /// `format_audit_line` green while the reader silently stops finding `defect=`
    /// and every comparison degrades to the live re-tally — the drift the snapshot
    /// exists to prevent. This closes the loop.
    #[test]
    fn the_written_line_is_readable_by_the_parser() {
        let counts = TaskCounts {
            pass: 6,
            defect: 3,
            scenario_error: 1,
            total: 10,
        };
        let line = format_audit_line(4, &counts, "continue");
        assert_eq!(
            parse_logged_round_defects(&line, 4),
            Some(3),
            "round-trip must hold; got line: {line}"
        );
        assert_eq!(
            parse_logged_round_defects(&line, 3),
            None,
            "and must not match a different round"
        );
    }

    /// Two projects can use the same domain name, and the baseline must not cross
    /// that boundary — it would put another project's defect count into a durable
    /// `regression` verdict.
    #[test]
    fn audit_titles_are_scoped_per_project() {
        let a = audit_log_title("PROJ-a", "Dogfood");
        let b = audit_log_title("PROJ-b", "Dogfood");
        assert_ne!(a, b, "same domain, different project → different entry");
        assert!(a.contains("PROJ-a") && a.contains("Dogfood"), "got: {a}");
    }

    /// The audit line is written by `convergence_status` as
    /// `R<n>: defect=X, scenario_error=Y, pass=Z, decision=…`. Regression detection
    /// reads its baseline back out of it, so a format drift would silently make
    /// every comparison fall back to the live re-tally — the instability the
    /// snapshot exists to remove. These pin the shape.
    #[test]
    fn reads_the_defect_count_a_round_reported() {
        let body = "R1: defect=5, scenario_error=0, pass=3, decision=continue\n\
                    R2: defect=2, scenario_error=1, pass=6, decision=continue";
        assert_eq!(parse_logged_round_defects(body, 1), Some(5));
        assert_eq!(parse_logged_round_defects(body, 2), Some(2));
    }

    #[test]
    fn a_rerun_appends_and_the_newest_line_wins() {
        // A round re-run logs again; the later line is what it last reported.
        let body = "R1: defect=5, scenario_error=0, pass=3, decision=continue\n\
                    R1: defect=1, scenario_error=0, pass=7, decision=continue";
        assert_eq!(parse_logged_round_defects(body, 1), Some(1));
    }

    #[test]
    fn round_numbers_are_not_matched_by_prefix() {
        // "R1:" must not match the R10 line — otherwise round 1's baseline would
        // silently come from round 10.
        let body = "R10: defect=9, scenario_error=0, pass=0, decision=continue";
        assert_eq!(parse_logged_round_defects(body, 1), None);
        assert_eq!(parse_logged_round_defects(body, 10), Some(9));
    }

    #[test]
    fn absent_or_malformed_yields_none_so_the_caller_falls_back() {
        assert_eq!(parse_logged_round_defects("", 1), None);
        assert_eq!(parse_logged_round_defects("R2: defect=1", 1), None);
        assert_eq!(
            parse_logged_round_defects("R1: scenario_error=0, pass=3", 1),
            None,
            "no defect field"
        );
        assert_eq!(
            parse_logged_round_defects("R1: defect=oops, pass=3", 1),
            None,
            "unparsable count"
        );
    }
}
