use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Timestamp helpers (FIX-DAEMON-013)
// Epoch-ms stored in SQLite; serialized as ISO 8601 UTC in JSON responses.
// ---------------------------------------------------------------------------

/// Serialize epoch-milliseconds as an ISO 8601 string ("2026-05-04T12:00:00.000Z").
pub fn ms_to_iso(ms: i64) -> String {
    let subsec_ms = (ms % 1000).unsigned_abs() as u32;
    // Build ISO 8601 via simple formatting (no chrono dep needed).
    // We use the UNIX epoch offset manually.
    // For simplicity we delegate to a const-time approach via formatting.
    // This is accurate for positive epoch-ms values.
    let dt = epoch_ms_to_parts(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second, subsec_ms
    )
}

struct DateTimeParts {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn epoch_ms_to_parts(ms: i64) -> DateTimeParts {
    let secs = ms / 1000;
    // Days since Unix epoch
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;

    // Compute year/month/day from days since 1970-01-01
    // Using the proleptic Gregorian calendar algorithm
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    DateTimeParts {
        year: y as i32,
        month: m as u32,
        day: d as u32,
        hour,
        minute,
        second,
    }
}

/// Serde helper: serialize i64 epoch-ms field as ISO 8601 string
pub mod ts_iso {
    use serde::{self, Serializer};
    pub fn serialize<S: Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::ms_to_iso(*ms))
    }
}

/// Serde helper: serialize Option<i64> epoch-ms field as Option<ISO 8601 string>
pub mod opt_ts_iso {
    use serde::{self, Serializer};
    pub fn serialize<S: Serializer>(ms: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
        match ms {
            Some(v) => s.serialize_some(&super::ms_to_iso(*v)),
            None => s.serialize_none(),
        }
    }
}

// ---------------------------------------------------------------------------
// AuditLogEntry (FIX-DAEMON-014) — replaces ActivityLogEntry for new code
// ---------------------------------------------------------------------------

/// Actor enum for audit_log.actor (FIX-DAEMON-014)
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Actor {
    Claude,
    Cli,
    #[serde(rename = "external-api")]
    ExternalApi,
    System,
}

#[allow(dead_code)]
impl Actor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Actor::Claude => "claude",
            Actor::Cli => "cli",
            Actor::ExternalApi => "external-api",
            Actor::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "claude" => Actor::Claude,
            "external-api" => Actor::ExternalApi,
            "system" => Actor::System,
            _ => Actor::Cli,
        }
    }
}

/// OpType enum for audit_log.op_type (FIX-DAEMON-014)
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpType {
    StatusChange,
    Created,
    Deleted,
    Updated,
    Approved,
    Activated,
    Completed,
    PolicyViolation,
}

#[allow(dead_code)]
impl OpType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OpType::StatusChange => "status_change",
            OpType::Created => "created",
            OpType::Deleted => "deleted",
            OpType::Updated => "updated",
            OpType::Approved => "approved",
            OpType::Activated => "activated",
            OpType::Completed => "completed",
            OpType::PolicyViolation => "POLICY_VIOLATION",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "status_change" => OpType::StatusChange,
            "created" => OpType::Created,
            "deleted" => OpType::Deleted,
            "approved" => OpType::Approved,
            "activated" => OpType::Activated,
            "completed" => OpType::Completed,
            "POLICY_VIOLATION" => OpType::PolicyViolation,
            _ => OpType::Updated,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub op_type: String,
    pub field: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: String,
    /// ISO 8601 UTC string stored directly in DB
    pub at: String,
    /// FIX-DAEMON-107: chain hash of previous entry id (64-char hex, NULL for first)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// ActivityLogEntry — kept for backward compatibility with rollup job
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityLogEntry {
    pub id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub field: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// TaskRelation
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRelation {
    pub id: String,
    pub source_task_id: String,
    pub target_task_id: String,
    pub relation_type: String,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// TaskComment
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskComment {
    pub id: String,
    pub task_id: String,
    pub author: String,
    pub body: String,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Question
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub plan_id: Option<String>,
    pub unit_id: Option<String>,
    pub task_id: Option<String>,
    pub kind: String,
    pub origin: String,
    pub body: String,
    pub asked_by: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    pub answer: Option<String>,
    pub answered_by: Option<String>,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub answered_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// TaskEnvelope
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub id: String,
    pub task_id: String,
    pub version: i64,
    pub json: String,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub signed_at: i64,
    pub signed_by: String,
    pub superseded_by: Option<String>,
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub session_id: Option<String>,
    pub agent: String,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub started_at: i64,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub ended_at: Option<i64>,
    pub result: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_snapshot: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Knowledge — a knowledge entry. wiki_idx/wiki_depth define sibling order and
// depth within the wiki tree.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knowledge {
    pub id: String,
    pub task_id: Option<String>,
    pub unit_id: Option<String>,
    pub plan_id: Option<String>,
    #[serde(rename = "type")]
    pub type_: String,
    pub title: String,
    pub content: String,
    pub content_format: String,
    pub parent_id: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki_idx: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki_depth: Option<i64>,
    /// For `type == "decision"` knowledge entries, the parsed `## Decision` body section.
    /// `None` for non-decision types or when the section is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_text: Option<String>,
    /// For `type == "decision"` knowledge entries, the parsed `## Outcome` body section.
    /// `None` for non-decision types or when the section is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

// ---------------------------------------------------------------------------
// KnowledgeVersion
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeVersion {
    pub id: String,
    pub knowledge_id: String,
    pub version: i64,
    pub content: Option<String>,
    pub content_format: Option<String>,
    // ISO8601-TIMESTAMPS: serialize epoch-ms as ISO 8601 like every other model.
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub created_at: Option<i64>,
    pub created_by: Option<String>,
}

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub key: Option<String>,
    pub enabled: i64,
    pub wiki_paths: Vec<String>,
    pub cwds: Vec<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub description: Option<String>,
    pub source: String,
    pub source_path: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub approved_at: Option<i64>,
    pub status: String,
    /// Migration 027: opt-in Stop-hook auto-advance. When true, the Stop hook
    /// injects the next actionable step instead of letting the agent stop while
    /// the plan still has remaining work.
    #[serde(default)]
    pub auto_advance: bool,
}

// ---------------------------------------------------------------------------
// Unit (FIX-DAEMON-004: removed status/approval columns)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    pub plan_id: String,
    pub idx: i64,
    pub title: String,
    pub goal: Option<String>,
    pub execution_mode: String,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
}

// ---------------------------------------------------------------------------
// Cycle (FIX-DAEMON-002: unit_id added)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cycle {
    pub id: String,
    pub project_id: String,
    pub unit_id: Option<String>,
    pub idx: i64,
    pub title: String,
    pub goal: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub started_at: Option<i64>,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub ended_at: Option<i64>,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Task (FIX-DAEMON-001: tier; FIX-DAEMON-006: qa_status/scenario_id/defect_task/scenario_amendment;
//       US-CKT-SCHEMA-011/021: evidence/batch_id)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub unit_id: String,
    pub cycle_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub ticket_number: Option<String>,
    pub idx: i64,
    pub title: String,
    pub body: String,
    pub priority: String,
    pub complexity: Option<String>,
    pub estimated_edits: Option<i64>,
    #[serde(rename = "type")]
    pub type_: String,
    pub reporter: Option<String>,
    pub assignee: Option<String>,
    pub agent_id: Option<String>,
    #[serde(serialize_with = "ts_iso::serialize")]
    pub created_at: i64,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub started_at: Option<i64>,
    #[serde(serialize_with = "opt_ts_iso::serialize")]
    pub completed_at: Option<i64>,
    pub status: String,
    pub depends_on: Vec<String>,
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_envelope_id: Option<String>,
    #[serde(default = "default_atomic_size_hint")]
    pub atomic_size_hint: String,
    #[serde(default = "default_decomposition_policy")]
    pub decomposition_policy: String,
    // FIX-DAEMON-001: tier
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tier: Option<String>,
    // TIER-041: tier_used — the *executed* tier (vs declared `tier`).
    // When tier_used != tier, an escalation_reason must accompany the change.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tier_used: Option<String>,
    // TIER-043: escalation_reason — required when tier_used differs from tier.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub escalation_reason: Option<String>,
    // FIX-DAEMON-006: QA workflow fields
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub qa_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scenario_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub defect_task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scenario_amendment: Option<String>,
    // US-CKT-SCHEMA-011: evidence — file:line or reasoning summary (max 4 KiB, enforced at API layer)
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evidence: Option<String>,
    // US-CKT-SCHEMA-021: batch_id — ULID identifying the sub-agent batch invocation
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub batch_id: Option<String>,
}

fn default_atomic_size_hint() -> String {
    "small".to_string()
}

fn default_decomposition_policy() -> String {
    "auto".to_string()
}

// ---------------------------------------------------------------------------
// PlanCounts / UnitCounts / CycleCounts (FIX-DAEMON-016 / FIX-DAEMON-101)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitCounts {
    pub unit_id: String,
    pub unit_title: String,
    pub todo: i64,
    pub in_progress: i64,
    pub done: i64,
    pub blocked: i64,
    pub cancelled: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCounts {
    pub plan_id: String,
    pub units: Vec<UnitCounts>,
    pub total_todo: i64,
    pub total_in_progress: i64,
    pub total_done: i64,
    pub total_blocked: i64,
    pub total_cancelled: i64,
    pub total_tasks: i64,
}

/// FIX-DAEMON-101: aggregate task counts scoped to a single unit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleUnitCounts {
    pub unit_id: String,
    pub unit_title: String,
    pub todo: i64,
    pub in_progress: i64,
    pub done: i64,
    pub blocked: i64,
    pub cancelled: i64,
    pub total: i64,
}

/// FIX-DAEMON-101: aggregate task counts scoped to a single cycle
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleCounts {
    pub cycle_id: String,
    pub cycle_title: String,
    pub todo: i64,
    pub in_progress: i64,
    pub done: i64,
    pub blocked: i64,
    pub cancelled: i64,
    pub total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_to_iso_epoch() {
        // Unix epoch = 1970-01-01T00:00:00.000Z
        assert_eq!(ms_to_iso(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn ms_to_iso_known_date() {
        // 2025-05-04T00:00:00.000Z = 1746316800000 ms
        let ms: i64 = 1746316800000;
        let s = ms_to_iso(ms);
        assert!(s.starts_with("2025-05-04T"), "got: {s}");
        assert!(s.ends_with('Z'), "got: {s}");
    }

    #[test]
    fn ms_to_iso_subsecond() {
        // 1 second + 500 ms
        let ms: i64 = 1500;
        let s = ms_to_iso(ms);
        assert!(s.ends_with(".500Z"), "got: {s}");
    }
}
