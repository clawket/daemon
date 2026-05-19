//! LM-151 / RL-U6-02 — pure structural validator for ADR-0001
//! envelopes. Operates on a JSON `Value` and emits a deterministic
//! `Vec<Violation>`; no DB access, no envelope resolution. Inheritance
//! resolution lives in `task_envelopes::resolve_chain`; this module
//! takes whatever shape callers provide (active envelope, resolved
//! envelope, or an unsigned draft from the web form) and reports
//! contract violations.
//!
//! Single source of truth for "is this envelope structurally valid":
//!   * MCP `clawket_validate_envelope` (CLI) — calls via HTTP route.
//!   * HTTP `POST /tasks/:id/envelope/validate` (this crate) — used by
//!     the web envelope form for real-time feedback (debounced 400ms).
//!
//! The validator is deliberately conservative: it reports violations
//! the daemon already enforces elsewhere (precondition syntax in
//! `envelope::conditions`, decomposition policy shape in route
//! handlers). Adding a check here without a corresponding enforcement
//! point creates lying UI — keep them in lockstep.

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Violation {
    pub field: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidateResult {
    pub valid: bool,
    pub strict: bool,
    pub violations: Vec<Violation>,
}

/// Validate an envelope JSON value. `strict=true` (the default for
/// the MCP tool) also reports recommended-but-missing fields as
/// warnings.
pub fn validate_envelope(envelope: &Value, strict: bool) -> ValidateResult {
    let mut v: Vec<Violation> = Vec::new();

    for required in ["intent", "prompt_template", "success_criteria"] {
        match envelope.get(required) {
            Some(Value::String(s)) if !s.trim().is_empty() => {}
            Some(Value::Array(arr)) if required == "success_criteria" && !arr.is_empty() => {}
            Some(Value::Null) | None => v.push(Violation {
                field: required.to_string(),
                severity: Severity::Error,
                message: format!("required field `{}` missing on resolved envelope", required),
            }),
            _ => v.push(Violation {
                field: required.to_string(),
                severity: Severity::Error,
                message: format!("required field `{}` must be a non-empty string", required),
            }),
        }
    }

    if let Some(policy) = envelope.get("decomposition_policy") {
        match policy {
            Value::String(s) => {
                if !matches!(s.as_str(), "auto" | "manual" | "atomic") {
                    v.push(Violation {
                        field: "decomposition_policy".into(),
                        severity: Severity::Error,
                        message: format!(
                            "decomposition_policy must be one of auto/manual/atomic, got `{}`",
                            s
                        ),
                    });
                }
            }
            Value::Object(obj) => {
                if let Some(md) = obj.get("max_depth") {
                    if !md.is_number() || md.as_i64().is_none_or(|n| !(1..=10).contains(&n)) {
                        v.push(Violation {
                            field: "decomposition_policy.max_depth".into(),
                            severity: Severity::Error,
                            message: "max_depth must be integer in [1,10]".into(),
                        });
                    }
                }
                if let Some(strat) = obj.get("strategy") {
                    if let Some(s) = strat.as_str() {
                        if !matches!(s, "auto" | "scoped" | "by-repo" | "manual") {
                            v.push(Violation {
                                field: "decomposition_policy.strategy".into(),
                                severity: Severity::Warning,
                                message: format!(
                                    "unknown strategy `{}` (expected: auto, scoped, by-repo, manual)",
                                    s
                                ),
                            });
                        }
                    }
                }
            }
            _ => v.push(Violation {
                field: "decomposition_policy".into(),
                severity: Severity::Error,
                message: "decomposition_policy must be a string or object".into(),
            }),
        }
    } else if strict {
        v.push(Violation {
            field: "decomposition_policy".into(),
            severity: Severity::Warning,
            message: "decomposition_policy missing — recommended for tree-structured tasks".into(),
        });
    }

    if let Some(hint) = envelope.get("atomic_size_hint") {
        match hint.as_str() {
            Some("tiny" | "small" | "medium" | "large") => {}
            Some(other) => v.push(Violation {
                field: "atomic_size_hint".into(),
                severity: Severity::Error,
                message: format!(
                    "atomic_size_hint must be one of tiny/small/medium/large, got `{}`",
                    other
                ),
            }),
            None => v.push(Violation {
                field: "atomic_size_hint".into(),
                severity: Severity::Error,
                message: "atomic_size_hint must be a string".into(),
            }),
        }
    }

    for cond_field in ["preconditions", "postconditions"] {
        if let Some(arr) = envelope.get(cond_field) {
            if let Some(items) = arr.as_array() {
                for (i, c) in items.iter().enumerate() {
                    match c {
                        Value::String(s) if !s.trim().is_empty() => {
                            if !is_balanced_parens(s) {
                                v.push(Violation {
                                    field: format!("{}[{}]", cond_field, i),
                                    severity: Severity::Error,
                                    message: "unbalanced parentheses in expression".into(),
                                });
                            }
                        }
                        _ => v.push(Violation {
                            field: format!("{}[{}]", cond_field, i),
                            severity: Severity::Error,
                            message: "condition must be a non-empty expression string".into(),
                        }),
                    }
                }
            } else {
                v.push(Violation {
                    field: cond_field.into(),
                    severity: Severity::Error,
                    message: format!("{} must be an array of expression strings", cond_field),
                });
            }
        }
    }

    for num_field in ["max_turns", "checkpoint_interval", "version"] {
        if let Some(val) = envelope.get(num_field) {
            if !val.is_number() {
                v.push(Violation {
                    field: num_field.into(),
                    severity: Severity::Error,
                    message: format!("{} must be a number", num_field),
                });
            }
        }
    }

    if strict {
        for rec in ["target_model", "max_turns"] {
            if envelope.get(rec).is_none() {
                v.push(Violation {
                    field: rec.into(),
                    severity: Severity::Warning,
                    message: format!("recommended field `{}` missing (strict mode)", rec),
                });
            }
        }
    }

    let valid = !v.iter().any(|x| matches!(x.severity, Severity::Error));
    ValidateResult {
        valid,
        strict,
        violations: v,
    }
}

fn is_balanced_parens(s: &str) -> bool {
    let mut depth: i32 = 0;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fully_populated_envelope_passes() {
        let env = json!({
            "version": 1,
            "intent": "do thing",
            "prompt_template": "do the thing",
            "success_criteria": ["it works"],
            "target_model": "sonnet",
            "max_turns": 30,
            "atomic_size_hint": "small",
            "decomposition_policy": "auto",
        });
        let result = validate_envelope(&env, true);
        assert!(
            result.valid,
            "expected valid envelope: {:?}",
            result.violations
        );
        assert!(result.violations.is_empty());
    }

    #[test]
    fn missing_required_emits_error() {
        let env = json!({});
        let result = validate_envelope(&env, false);
        assert!(!result.valid);
        let fields: Vec<_> = result
            .violations
            .iter()
            .filter(|v| matches!(v.severity, Severity::Error))
            .map(|v| v.field.as_str())
            .collect();
        assert!(fields.contains(&"intent"));
        assert!(fields.contains(&"prompt_template"));
        assert!(fields.contains(&"success_criteria"));
    }

    #[test]
    fn success_criteria_array_form_accepted() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["one", "two"],
        });
        let result = validate_envelope(&env, false);
        assert!(result.valid, "{:?}", result.violations);
    }

    #[test]
    fn empty_success_criteria_array_is_invalid() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": [],
        });
        let result = validate_envelope(&env, false);
        assert!(!result.valid);
        assert!(result
            .violations
            .iter()
            .any(|v| v.field == "success_criteria"));
    }

    #[test]
    fn bad_atomic_size_hint_is_error() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
            "atomic_size_hint": "huge",
        });
        let result = validate_envelope(&env, false);
        assert!(!result.valid);
        let viol = result
            .violations
            .iter()
            .find(|v| v.field == "atomic_size_hint")
            .expect("expected atomic_size_hint violation");
        assert_eq!(viol.severity, Severity::Error);
    }

    #[test]
    fn unbalanced_precondition_emits_error() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
            "preconditions": ["status_eq(in_progress"],
        });
        let result = validate_envelope(&env, false);
        assert!(!result.valid);
        assert!(result
            .violations
            .iter()
            .any(|v| v.field == "preconditions[0]"));
    }

    #[test]
    fn strict_mode_warns_on_missing_recommended_fields() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
        });
        let result = validate_envelope(&env, true);
        // Still valid (no errors), but should have warnings.
        assert!(result.valid);
        let warnings: Vec<_> = result
            .violations
            .iter()
            .filter(|v| matches!(v.severity, Severity::Warning))
            .map(|v| v.field.as_str())
            .collect();
        assert!(warnings.contains(&"target_model"));
        assert!(warnings.contains(&"max_turns"));
        assert!(warnings.contains(&"decomposition_policy"));
    }

    #[test]
    fn non_strict_mode_skips_recommended_warnings() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
        });
        let result = validate_envelope(&env, false);
        assert!(result.valid);
        let warnings: Vec<_> = result
            .violations
            .iter()
            .filter(|v| matches!(v.severity, Severity::Warning))
            .collect();
        assert!(
            warnings.is_empty(),
            "non-strict should not warn about recommended fields: {:?}",
            warnings
        );
    }

    #[test]
    fn numeric_fields_must_be_numbers() {
        let env = json!({
            "intent": "x",
            "prompt_template": "y",
            "success_criteria": ["z"],
            "max_turns": "thirty",
        });
        let result = validate_envelope(&env, false);
        assert!(!result.valid);
        assert!(result.violations.iter().any(|v| v.field == "max_turns"));
    }
}
