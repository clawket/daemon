use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::Value;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub code: Option<String>,
    pub details: Option<Value>,
    /// US-CLAWKET-PDD-111: when true and `details` is a JSON object, the
    /// detail fields are flattened into the top-level error body instead of
    /// being wrapped under `details`. This lets specific contracts return
    /// shapes like `{ error: "single_active_plan", existing_plan_id: "..." }`
    /// without retrofitting the global error envelope.
    pub flat_details: bool,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: None,
            details: None,
            flat_details: false,
        }
    }

    pub fn bad_request_coded(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: Some(code.to_string()),
            details: None,
            flat_details: false,
        }
    }

    /// 400 with a structured `details` payload — used by strict-mode plan
    /// import to surface line/column/kind/hint to the hook.
    pub fn bad_request_with_details(msg: impl Into<String>, details: Value) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: None,
            details: Some(details),
            flat_details: false,
        }
    }

    /// 400 carrying BOTH a stable `code` and a structured `details` payload.
    /// #11: the aggregated task-create validation response uses this to keep the
    /// top-level `error`/`code` identical to the legacy single-gate response
    /// (backward compatibility) while attaching every violation under
    /// `details.violations`.
    pub fn bad_request_coded_with_details(
        code: impl Into<String>,
        msg: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: Some(code.into()),
            details: Some(details),
            flat_details: false,
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: None,
            details: None,
            flat_details: false,
        }
    }

    pub fn not_found_coded(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: Some(code.to_string()),
            details: None,
            flat_details: false,
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            code: None,
            details: None,
            flat_details: false,
        }
    }

    pub fn conflict_coded(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            code: Some(code.to_string()),
            details: None,
            flat_details: false,
        }
    }

    /// Conflict response with a structured `details` payload — used by
    /// envelope condition violations so callers can identify the
    /// offending predicate without parsing the human-readable message.
    pub fn conflict_with_details(msg: impl Into<String>, details: Value) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            code: None,
            details: Some(details),
            flat_details: false,
        }
    }

    /// US-CLAWKET-PDD-111: 409 conflict whose JSON body flattens its
    /// `details` object into the top-level error envelope. Used for
    /// contract-shaped errors (e.g. `{ error: "single_active_plan",
    /// existing_plan_id: "<id>" }`).
    pub fn conflict_flat(msg: impl Into<String>, details: Value) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
            code: None,
            details: Some(details),
            flat_details: true,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            code: None,
            details: None,
            flat_details: false,
        }
    }

    /// US-CKT-SCHEMA-037: 503 returned to clients while a schema migration
    /// is in flight. Distinct from SQLITE_BUSY — this signals a bounded
    /// "wait for the daemon to finish migrating" rather than a transient lock.
    pub fn migration_in_progress() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "schema migration in progress".to_string(),
            code: Some("MIGRATION_IN_PROGRESS".to_string()),
            details: None,
            flat_details: false,
        }
    }
}

impl From<crate::envelope::sign::EnvelopeSignError> for ApiError {
    fn from(err: crate::envelope::sign::EnvelopeSignError) -> Self {
        use crate::envelope::sign::EnvelopeSignError;
        match err {
            EnvelopeSignError::Validation(violations) => {
                let summary = violations
                    .iter()
                    .map(|v| format!("{}: {}", v.field, v.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                let details = serde_json::json!({ "violations": violations });
                ApiError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("ENVELOPE_INVALID: {summary}"),
                    code: Some("ENVELOPE_INVALID".to_string()),
                    details: Some(details),
                    flat_details: false,
                }
            }
            EnvelopeSignError::Storage(e) => ApiError::from(e),
        }
    }
}

/// Single source of truth for mapping a concrete `rusqlite::Error` to an
/// `ApiError`. Returns `Some` only for the cases that carry a meaningful HTTP
/// status (currently SQLITE_BUSY/LOCKED → 503, retryable); `None` means "no
/// specific mapping — let the caller decide the fallback". Both the direct
/// `From<rusqlite::Error>` and the `From<anyhow::Error>` chain-walk go through
/// this, so a BUSY error maps to 503 whether or not a repo layer wrapped it in
/// `.context(...)` (#10: the anyhow path previously truncated the root cause and
/// fell through to an uninformative 500).
fn map_rusqlite_status(err: &rusqlite::Error) -> Option<ApiError> {
    if matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                ..
            },
            _
        ) | rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    ) {
        return Some(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("SQLITE_BUSY: database is busy, retry shortly: {err}"),
            code: Some("SQLITE_BUSY".to_string()),
            details: None,
            flat_details: false,
        });
    }
    None
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        // SQLite BUSY/LOCKED → 503 (retryable)
        if let Some(api) = map_rusqlite_status(&err) {
            return api;
        }
        ApiError::internal(err.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let msg = err.to_string();
        // Structured error codes — check before generic heuristics
        // US-CLAWKET-I18N-040: TASK_NOT_FOUND is a localizable 404. The
        // human-readable suffix preserved here (after the code) carries the
        // task id so callers debugging in English can still read it; the
        // localized prefix is used for display in Korean/Japanese clients.
        if msg.starts_with("TASK_NOT_FOUND:") {
            return ApiError::not_found_coded("TASK_NOT_FOUND", msg);
        }
        if msg.starts_with("PROJECT_NOT_FOUND:") {
            return ApiError::not_found_coded("PROJECT_NOT_FOUND", msg);
        }
        if msg.starts_with("MISSING_PROJECT:") {
            return ApiError::bad_request_coded("MISSING_PROJECT", msg);
        }
        if msg.starts_with("MISSING_UNIT_ID:") {
            return ApiError::bad_request_coded("MISSING_UNIT_ID", msg);
        }
        if msg.starts_with("UNIT_NOT_FOUND:") {
            return ApiError::not_found_coded("UNIT_NOT_FOUND", msg);
        }
        if msg.starts_with("UNIT_HAS_ACTIVE_CYCLE:") {
            return ApiError::conflict_coded("UNIT_HAS_ACTIVE_CYCLE", msg);
        }
        if msg.starts_with("PREVIOUS_CYCLE_NOT_DONE:") {
            return ApiError::conflict_coded("PREVIOUS_CYCLE_NOT_DONE", msg);
        }
        if msg.starts_with("INVALID_TIER:") {
            return ApiError::bad_request_coded("INVALID_TIER", msg);
        }
        if msg.starts_with("INVALID_QA_STATUS:") {
            return ApiError::bad_request_coded("INVALID_QA_STATUS", msg);
        }
        if msg.starts_with("INVALID_TITLE:") {
            return ApiError::bad_request_coded("INVALID_TITLE", msg);
        }
        if msg.starts_with("CYCLE_UNIT_MISMATCH:") {
            return ApiError::bad_request_coded("CYCLE_UNIT_MISMATCH", msg);
        }
        if msg.starts_with("BLOCKED_REASON_REQUIRED:") {
            return ApiError::bad_request_coded("BLOCKED_REASON_REQUIRED", msg);
        }
        if msg.starts_with("EVIDENCE_REQUIRED:") {
            return ApiError::bad_request_coded("EVIDENCE_REQUIRED", msg);
        }
        if msg.starts_with("PUBLIC_BIND_NOT_ALLOWED:") {
            return ApiError::bad_request_coded("PUBLIC_BIND_NOT_ALLOWED", msg);
        }
        if msg.starts_with("SCOPE_DEPRECATED:") {
            return ApiError::bad_request_coded("SCOPE_DEPRECATED", msg);
        }
        if msg.starts_with("KNOWLEDGE_NEEDS_ATTACHMENT:") {
            return ApiError::bad_request_coded("KNOWLEDGE_NEEDS_ATTACHMENT", msg);
        }
        if msg.starts_with("MISSING_CYCLE_ID:") {
            return ApiError::bad_request_coded("MISSING_CYCLE_ID", msg);
        }
        if msg.starts_with("INVALID_TRANSITION:") {
            return ApiError::bad_request_coded("INVALID_TRANSITION", msg);
        }
        if msg.starts_with("BLOCKED_BY_DEPENDENCY:") {
            return ApiError::conflict_coded("BLOCKED_BY_DEPENDENCY", msg);
        }
        if msg.starts_with("TICKET_KEY_CONFLICT:") {
            return ApiError::conflict_coded("TICKET_KEY_CONFLICT", msg);
        }
        if msg.starts_with("CWD_ALREADY_REGISTERED:") {
            return ApiError::conflict_coded("CWD_ALREADY_REGISTERED", msg);
        }
        if msg.starts_with("RUN_ALREADY_OPEN:") {
            return ApiError::conflict_coded("RUN_ALREADY_OPEN", msg);
        }
        // PDD-230: cycle complete rejected because of non-terminal tasks
        if msg.starts_with("CYCLE_HAS_NON_TERMINAL_TASKS:") {
            return ApiError::conflict_coded("CYCLE_HAS_NON_TERMINAL_TASKS", msg);
        }
        // PDD-111: project already has active plan
        if msg.starts_with("PROJECT_HAS_ACTIVE_PLAN:") {
            return ApiError::conflict_coded("PROJECT_HAS_ACTIVE_PLAN", msg);
        }
        // PDD-231: plan complete rejected because of remaining active cycles
        if msg.starts_with("PLAN_HAS_ACTIVE_CYCLES:") {
            return ApiError::conflict_coded("PLAN_HAS_ACTIVE_CYCLES", msg);
        }
        // LM-11031: task/unit create rejected because parent plan is completed.
        // 409 (not 400) — the request itself is well-formed; the conflict is
        // with the structural invariant that completed plans are frozen.
        if msg.starts_with("PLAN_COMPLETED:") {
            return ApiError::conflict_coded("PLAN_COMPLETED", msg);
        }
        // API-CYCLE-005: cycles must be planned in order
        if msg.starts_with("INVALID_REQUEST:") {
            return ApiError::bad_request_coded("INVALID_REQUEST", msg);
        }
        // TIER-043: escalation reason required
        if msg.starts_with("ESCALATION_REASON_REQUIRED:") {
            return ApiError::bad_request_coded("ESCALATION_REASON_REQUIRED", msg);
        }
        // ── /backup, /restore (repo::backup) ──────────────────────────────
        // Bad user-supplied path (relative, empty, NUL byte) — the request
        // itself is malformed, so 400.
        if msg.starts_with("INVALID_BACKUP_PATH:") {
            return ApiError::bad_request_coded("INVALID_BACKUP_PATH", msg);
        }
        // The named archive is not there — 404 matches the other *_NOT_FOUND
        // codes in this ladder.
        if msg.starts_with("BACKUP_ARCHIVE_NOT_FOUND:") {
            return ApiError::not_found_coded("BACKUP_ARCHIVE_NOT_FOUND", msg);
        }
        // Archive is present but unreadable / not a Clawket backup / payload
        // fails integrity_check. 400: the caller handed us bad input.
        if msg.starts_with("BACKUP_ARCHIVE_INVALID:") {
            return ApiError::bad_request_coded("BACKUP_ARCHIVE_INVALID", msg);
        }
        // `--merge` is refused outright rather than silently downgraded to a
        // replace. 400 so the CLI surfaces the reason verbatim.
        if msg.starts_with("MERGE_NOT_SUPPORTED:") {
            return ApiError::bad_request_coded("MERGE_NOT_SUPPORTED", msg);
        }
        // Archive was produced by a newer schema than this binary understands.
        // Deliberately NOT db.rs's SCHEMA_DOWNGRADE_REFUSED: that one aborts
        // startup over the daemon's own store, this rejects one request over a
        // caller-supplied file. The daemon stays up and another archive may
        // work, so a client must be able to tell them apart.
        if msg.starts_with("ARCHIVE_SCHEMA_TOO_NEW:") {
            return ApiError::bad_request_coded("ARCHIVE_SCHEMA_TOO_NEW", msg);
        }
        // Backup could not be produced (e.g. header overflow). 500: the
        // request was fine; the daemon failed. Built by hand rather than via
        // `ApiError::internal`, which drops `code` — the same reason
        // SQLITE_BUSY below does. A 500 is still an error clients branch on,
        // and every other prefix in this ladder carries its code.
        if msg.starts_with("BACKUP_FAILED:") {
            return ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: msg,
                code: Some("BACKUP_FAILED".to_string()),
                details: None,
                flat_details: false,
            };
        }
        if msg.starts_with("SQLITE_BUSY:") {
            return ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: msg,
                code: Some("SQLITE_BUSY".to_string()),
                details: None,
                flat_details: false,
            };
        }
        // #10: a repo layer that wraps a rusqlite error in `.context("insert
        // task")` truncates the SQLite root cause when only the outermost
        // context string is matched above. Walk the anyhow chain for the
        // concrete `rusqlite::Error` and route it through the SAME status
        // mapping as `From<rusqlite::Error>`, so an anyhow-wrapped BUSY becomes
        // 503 (retryable) instead of an opaque 500.
        if let Some(rsq) = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<rusqlite::Error>())
        {
            if let Some(api) = map_rusqlite_status(rsq) {
                return api;
            }
        }
        // Generic heuristics
        if msg.contains("not found") || msg.contains("Not found") {
            return ApiError::not_found(msg);
        }
        if msg.contains("cannot complete cycle:") {
            return ApiError::conflict_coded("CYCLE_HAS_RESIDUE", msg);
        }
        if msg.contains("Invalid")
            || msg.contains("required")
            || msg.contains("Cannot")
            || msg.contains("draft plan")
            || msg.contains("cannot be restarted")
        {
            return ApiError::bad_request(msg);
        }
        // #10: surface the FULL anyhow chain (alternate format) instead of just
        // the outermost context, so an otherwise-opaque 500 (e.g. "insert task")
        // still carries the underlying SQLite/root-cause detail for diagnosis.
        ApiError::internal(format!("{err:#}"))
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<String>,
}

fn debug_enabled() -> bool {
    std::env::var("CLAWKET_DEBUG")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let stack = if debug_enabled() {
            Some(format!("{}\n(status={})", self.message, self.status))
        } else {
            None
        };
        // US-CLAWKET-PDD-111: when `flat_details` is set, fold the details
        // object into the top-level error envelope. Falls back to the wrapped
        // form for non-object details or callers that did not opt in.
        if self.flat_details {
            if let Some(Value::Object(detail_obj)) = self.details.as_ref() {
                let mut map = serde_json::Map::new();
                map.insert("error".into(), Value::String(self.message.clone()));
                if let Some(c) = self.code.as_ref() {
                    map.insert("code".into(), Value::String(c.clone()));
                }
                for (k, v) in detail_obj.iter() {
                    // Don't let detail keys clobber `error` or `code`.
                    if k == "error" || k == "code" {
                        continue;
                    }
                    map.insert(k.clone(), v.clone());
                }
                if let Some(s) = stack.as_ref() {
                    map.insert("stack".into(), Value::String(s.clone()));
                }
                return (self.status, Json(Value::Object(map))).into_response();
            }
        }
        let body = ErrorBody {
            error: self.message,
            code: self.code,
            details: self.details,
            stack,
        };
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn json_or_404<T: Serialize>(value: Option<T>) -> ApiResult<Json<T>> {
    value
        .map(Json)
        .ok_or_else(|| ApiError::not_found("not found"))
}

/// I18N-040: localize an error message by code + locale.
/// Returns a translated message string. Falls back to English when the (code, lang) pair
/// is unknown. Caller is expected to use this for the human-readable `message` field;
/// the structured `code` field always remains stable for machine consumers.
///
/// Locale matching is a simple prefix check ("ko" matches "ko-KR", etc).
pub fn localize_error(code: &str, lang: &str) -> String {
    let lang2 = lang.split('-').next().unwrap_or("en");
    // US-CLAWKET-I18N-040: also accept dotted keys like "error.task.not_found"
    // so callers can use either the screaming-snake code form or the rule-defined
    // dotted form interchangeably.
    let normalized = match code {
        "error.task.not_found" => "TASK_NOT_FOUND",
        other => other,
    };
    match (normalized, lang2) {
        ("TASK_NOT_FOUND", "ko") => "TASK_NOT_FOUND: 작업을 찾을 수 없습니다.".into(),
        ("TASK_NOT_FOUND", "ja") => "TASK_NOT_FOUND: タスクが見つかりません。".into(),
        ("TASK_NOT_FOUND", _)    => "TASK_NOT_FOUND: task not found.".into(),

        ("MISSING_CYCLE_ID", "ko") => "MISSING_CYCLE_ID: 작업 생성 시 cycle_id 가 필요합니다.".into(),
        ("MISSING_CYCLE_ID", "ja") => "MISSING_CYCLE_ID: タスク作成には cycle_id が必要です。".into(),
        ("MISSING_CYCLE_ID", _)    => "MISSING_CYCLE_ID: cycle_id is required when creating a task.".into(),

        ("PROJECT_HAS_ACTIVE_PLAN", "ko") => "PROJECT_HAS_ACTIVE_PLAN: 프로젝트에 이미 활성 플랜이 있습니다.".into(),
        ("PROJECT_HAS_ACTIVE_PLAN", "ja") => "PROJECT_HAS_ACTIVE_PLAN: プロジェクトには既にアクティブなプランがあります。".into(),
        ("PROJECT_HAS_ACTIVE_PLAN", _)    => "PROJECT_HAS_ACTIVE_PLAN: project already has an active plan.".into(),

        ("CYCLE_HAS_NON_TERMINAL_TASKS", "ko") => "CYCLE_HAS_NON_TERMINAL_TASKS: 종료되지 않은 task 가 있어 cycle 을 완료할 수 없습니다.".into(),
        ("CYCLE_HAS_NON_TERMINAL_TASKS", "ja") => "CYCLE_HAS_NON_TERMINAL_TASKS: 未完了のタスクがあるため、サイクルを完了できません。".into(),
        ("CYCLE_HAS_NON_TERMINAL_TASKS", _)    => "CYCLE_HAS_NON_TERMINAL_TASKS: cannot complete cycle: non-terminal tasks remain.".into(),

        ("ESCALATION_REASON_REQUIRED", "ko") => "ESCALATION_REASON_REQUIRED: tier_used 가 tier 와 다를 때는 escalation_reason 이 필요합니다.".into(),
        ("ESCALATION_REASON_REQUIRED", "ja") => "ESCALATION_REASON_REQUIRED: tier_used が tier と異なる場合は escalation_reason が必要です。".into(),
        ("ESCALATION_REASON_REQUIRED", _)    => "ESCALATION_REASON_REQUIRED: escalation_reason is required when tier_used differs from tier.".into(),

        // Default — pass code through as the message.
        (other, _) => other.to_string(),
    }
}
