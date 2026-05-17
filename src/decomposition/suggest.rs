//! Pure rule-based decomposition suggester. Given a parent task's
//! resolved envelope and a few cheap counters, produce a deterministic
//! list of subtask suggestions plus any policy violations the caller
//! should surface to the user.
//!
//! No LLM call. No DB access. The route handler is responsible for
//! gathering inputs (envelope resolution, descendant count) and JSON-
//! serializing the result.

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize)]
pub struct ParentSummary {
    pub id: String,
    pub ticket_number: Option<String>,
    pub title: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    pub idx: usize,
    pub title: String,
    pub rationale: String,
    pub scope_hint: &'static str,
    pub inherited_envelope_keys: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PolicyViolation {
    pub field: &'static str,
    pub severity: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecompositionResult {
    pub parent: ParentSummary,
    pub strategy: String,
    pub max_depth: u32,
    pub existing_children_count: usize,
    pub suggested_subtasks: Vec<Suggestion>,
    pub policy_violations: Vec<PolicyViolation>,
}

/// Cap suggestion title length so the side panel doesn't get a
/// runaway one-liner that breaks layout.
const TITLE_TRUNCATE: usize = 80;
/// Cap rationale length similarly. Long rationales are kept in full
/// in the source criterion — the suggestion is just a pointer.
const RATIONALE_TRUNCATE: usize = 120;

/// Build subtask suggestions from a parent task's resolved envelope.
///
/// Inputs:
/// * `parent` — minimal identity for the suggestion result.
/// * `resolved` — the parent's resolved envelope (after inheritance
///   merge). May be `Value::Null` when the parent has no envelope.
/// * `existing_children_count` — used by the route handler to populate
///   the response; not consulted by the rule set yet but exposed so
///   future policies (e.g. "skip suggesting if already decomposed")
///   can be added without changing the call shape.
/// * `max_depth` — clamped to [1, 3] by the caller; reported back as
///   part of the response so the web side can render the cap.
/// * `strategy` — `auto` | `scoped` | `by-repo`. Unknown values fall
///   back to `auto`.
pub fn generate(
    parent: ParentSummary,
    resolved: &Value,
    existing_children_count: usize,
    max_depth: u32,
    strategy: &str,
) -> DecompositionResult {
    let strategy_canon = match strategy {
        "by-repo" | "scoped" => strategy.to_string(),
        _ => "auto".to_string(),
    };

    let scope_hint: &'static str = match strategy_canon.as_str() {
        "by-repo" => "scope by-repo (cli/daemon/web 분리)",
        "scoped" => "scope by surface (api|ui|infra)",
        _ => "auto",
    };

    let criteria_lines = parse_success_criteria(resolved);
    let mut suggestions: Vec<Suggestion> = criteria_lines
        .iter()
        .enumerate()
        .map(|(i, line)| Suggestion {
            idx: i,
            title: format!("{} — {}", parent.title, truncate(line, TITLE_TRUNCATE)),
            rationale: format!("success_criteria #{}: {}", i + 1, truncate(line, RATIONALE_TRUNCATE)),
            scope_hint,
            inherited_envelope_keys: vec!["intent", "prompt_template", "decomposition_policy"],
        })
        .collect();

    let mut violations: Vec<PolicyViolation> = Vec::new();
    let policy = resolved.get("decomposition_policy");
    match policy {
        Some(p) if !p.is_null() => {
            if let Some(min_n) = p.get("min_subtasks").and_then(Value::as_u64) {
                if (suggestions.len() as u64) < min_n {
                    violations.push(PolicyViolation {
                        field: "min_subtasks",
                        severity: "warning",
                        message: format!(
                            "only {} suggestions, min_subtasks={}",
                            suggestions.len(),
                            min_n
                        ),
                    });
                }
            }
            if let Some(max_n) = p.get("max_subtasks").and_then(Value::as_u64) {
                if (suggestions.len() as u64) > max_n {
                    violations.push(PolicyViolation {
                        field: "max_subtasks",
                        severity: "warning",
                        message: format!(
                            "{} suggestions exceed max_subtasks={}",
                            suggestions.len(),
                            max_n
                        ),
                    });
                    suggestions.truncate(max_n as usize);
                }
            }
            if let Some(pmd) = p.get("max_depth").and_then(Value::as_u64) {
                if (max_depth as u64) > pmd {
                    violations.push(PolicyViolation {
                        field: "max_depth",
                        severity: "error",
                        message: format!(
                            "requested max_depth={} exceeds policy max_depth={}",
                            max_depth, pmd
                        ),
                    });
                }
            }
        }
        _ => {
            violations.push(PolicyViolation {
                field: "decomposition_policy",
                severity: "warning",
                message: "no decomposition_policy on resolved envelope — using default".to_string(),
            });
        }
    }

    if criteria_lines.is_empty() {
        violations.push(PolicyViolation {
            field: "success_criteria",
            severity: "error",
            message: "no parseable success_criteria — cannot derive subtasks. Add bullet list under success_criteria.".to_string(),
        });
    }

    DecompositionResult {
        parent,
        strategy: strategy_canon,
        max_depth,
        existing_children_count,
        suggested_subtasks: suggestions,
        policy_violations: violations,
    }
}

/// Convert a `DecompositionResult` to the wire JSON shape the MCP and
/// web clients have consumed since the original CLI implementation.
/// We could derive Serialize directly but the legacy keys (`scope_hint`,
/// `inherited_envelope_keys`) and the field order matter for snapshot
/// tests downstream — keeping the shaping explicit avoids surprise.
pub fn to_json(result: &DecompositionResult) -> Value {
    json!({
        "parent": {
            "id": result.parent.id,
            "ticket_number": result.parent.ticket_number,
            "title": result.parent.title,
        },
        "strategy": result.strategy,
        "max_depth": result.max_depth,
        "existing_children_count": result.existing_children_count,
        "suggested_subtasks": result.suggested_subtasks.iter().map(|s| json!({
            "idx": s.idx,
            "title": s.title,
            "rationale": s.rationale,
            "scope_hint": s.scope_hint,
            "inherited_envelope_keys": s.inherited_envelope_keys,
        })).collect::<Vec<_>>(),
        "policy_violations": result.policy_violations.iter().map(|v| json!({
            "field": v.field,
            "severity": v.severity,
            "message": v.message,
        })).collect::<Vec<_>>(),
    })
}

/// `success_criteria` may be a newline-separated string or a JSON
/// array of strings. Both are valid envelope shapes — the form's
/// `list` widget produces arrays, but legacy hand-written envelopes
/// often use a string with bullet markers. Normalize to `Vec<String>`.
fn parse_success_criteria(resolved: &Value) -> Vec<String> {
    let raw = resolved.get("success_criteria");
    let Some(raw) = raw else { return Vec::new() };

    if let Some(arr) = raw.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| strip_bullet(s).to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if let Some(s) = raw.as_str() {
        return s
            .lines()
            .map(|l| strip_bullet(l).to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    Vec::new()
}

fn strip_bullet(line: &str) -> &str {
    line.trim_start_matches(['-', '*', ' ', '\t']).trim()
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}\u{2026}")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parent() -> ParentSummary {
        ParentSummary {
            id: "TASK-1".to_string(),
            ticket_number: Some("LM-1".to_string()),
            title: "Build the thing".to_string(),
        }
    }

    #[test]
    fn array_success_criteria_yields_one_suggestion_per_item() {
        let resolved = json!({
            "success_criteria": ["criterion A", "criterion B", "criterion C"]
        });
        let result = generate(parent(), &resolved, 0, 2, "auto");
        assert_eq!(result.suggested_subtasks.len(), 3);
        assert_eq!(result.suggested_subtasks[0].idx, 0);
        assert!(result.suggested_subtasks[0].title.contains("Build the thing"));
        assert!(result.suggested_subtasks[0].title.contains("criterion A"));
    }

    #[test]
    fn string_success_criteria_strips_bullets_and_blanks() {
        let resolved = json!({
            "success_criteria": "- alpha\n* beta\n\n  gamma\n"
        });
        let result = generate(parent(), &resolved, 0, 2, "auto");
        assert_eq!(result.suggested_subtasks.len(), 3);
        assert!(result.suggested_subtasks[0].title.contains("alpha"));
        assert!(result.suggested_subtasks[1].title.contains("beta"));
        assert!(result.suggested_subtasks[2].title.contains("gamma"));
    }

    #[test]
    fn missing_success_criteria_emits_error_violation() {
        let resolved = json!({});
        let result = generate(parent(), &resolved, 0, 2, "auto");
        assert!(result.suggested_subtasks.is_empty());
        let v = result
            .policy_violations
            .iter()
            .find(|v| v.field == "success_criteria")
            .expect("expected success_criteria violation");
        assert_eq!(v.severity, "error");
    }

    #[test]
    fn no_decomposition_policy_emits_warning() {
        let resolved = json!({ "success_criteria": ["a"] });
        let result = generate(parent(), &resolved, 0, 2, "auto");
        let v = result
            .policy_violations
            .iter()
            .find(|v| v.field == "decomposition_policy")
            .expect("expected decomposition_policy violation");
        assert_eq!(v.severity, "warning");
    }

    #[test]
    fn min_max_subtasks_bounds_emit_warnings() {
        let resolved = json!({
            "success_criteria": ["a", "b", "c", "d", "e"],
            "decomposition_policy": { "min_subtasks": 6, "max_subtasks": 3, "max_depth": 2 }
        });
        let result = generate(parent(), &resolved, 0, 2, "auto");
        // Truncated to max_subtasks=3, so post-truncation count is 3.
        // min_subtasks=6 vs *initial* length=5 → fires warning before truncation.
        assert_eq!(result.suggested_subtasks.len(), 3);
        let fields: Vec<&str> = result
            .policy_violations
            .iter()
            .map(|v| v.field)
            .collect();
        assert!(fields.contains(&"min_subtasks"));
        assert!(fields.contains(&"max_subtasks"));
    }

    #[test]
    fn max_depth_request_above_policy_emits_error() {
        let resolved = json!({
            "success_criteria": ["a"],
            "decomposition_policy": { "max_depth": 1 }
        });
        let result = generate(parent(), &resolved, 0, 3, "auto");
        let v = result
            .policy_violations
            .iter()
            .find(|v| v.field == "max_depth")
            .expect("expected max_depth violation");
        assert_eq!(v.severity, "error");
    }

    #[test]
    fn unknown_strategy_normalizes_to_auto() {
        let resolved = json!({ "success_criteria": ["a"] });
        let result = generate(parent(), &resolved, 0, 2, "weird-strategy");
        assert_eq!(result.strategy, "auto");
        assert_eq!(result.suggested_subtasks[0].scope_hint, "auto");
    }

    #[test]
    fn json_shape_includes_all_top_level_keys() {
        let resolved = json!({ "success_criteria": ["a"] });
        let result = generate(parent(), &resolved, 7, 2, "auto");
        let j = to_json(&result);
        assert!(j.get("parent").is_some());
        assert!(j.get("strategy").is_some());
        assert!(j.get("max_depth").is_some());
        assert!(j.get("existing_children_count").is_some());
        assert_eq!(j["existing_children_count"], 7);
        assert!(j.get("suggested_subtasks").is_some());
        assert!(j.get("policy_violations").is_some());
    }
}
