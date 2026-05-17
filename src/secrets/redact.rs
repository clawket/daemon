//! Heuristic detector for accidentally pasted secrets in envelope JSON.
//!
//! Strings whose Shannon entropy exceeds 4.5 bits per character AND whose
//! length exceeds 20 characters look like base64/hex API keys and almost
//! never appear in legitimate prose. The envelope create / update routes
//! reject any such leaf — users must move the value into a vault and
//! reference it through `secrets_ref`.

use serde_json::Value;

/// Threshold derived empirically from a handful of real keys + a typical
/// English-language envelope corpus. Natural language sits around
/// 4.0-4.4 bits/char; anything above 4.5 with ≥20 characters is suspect.
pub const ENTROPY_THRESHOLD: f64 = 4.5;
pub const MIN_LENGTH: usize = 20;

#[derive(Debug, PartialEq)]
pub struct EntropyHit {
    /// JSON pointer (`/foo/bar`) to the offending leaf.
    pub pointer: String,
    pub length: usize,
    pub entropy: f64,
}

/// Returns Shannon entropy in bits per character. Empty input → 0.0.
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0u32) += 1;
    }
    let len = s.chars().count() as f64;
    let mut h = 0.0f64;
    for &n in counts.values() {
        let p = n as f64 / len;
        h -= p * p.log2();
    }
    h
}

/// Walk every string leaf of `value` and return the first leaf whose
/// length AND entropy both exceed the thresholds. Returns `None` for
/// "envelope looks clean".
///
/// The walker only considers string leaves — numbers, booleans, and
/// nulls are skipped. Object keys are skipped too: a structure like
/// `"EXAMPLE_KEY_xxx": true` is unusual but not a meaningful exfil channel.
pub fn find_high_entropy(value: &Value) -> Option<EntropyHit> {
    let mut hit: Option<EntropyHit> = None;
    walk(value, String::new(), &mut hit);
    hit
}

fn walk(value: &Value, pointer: String, hit: &mut Option<EntropyHit>) {
    if hit.is_some() {
        return;
    }
    match value {
        Value::String(s) => {
            if s.len() > MIN_LENGTH {
                let h = shannon_entropy(s);
                if h > ENTROPY_THRESHOLD {
                    *hit = Some(EntropyHit {
                        pointer,
                        length: s.len(),
                        entropy: h,
                    });
                }
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let mut p = pointer.clone();
                p.push('/');
                p.push_str(&i.to_string());
                walk(v, p, hit);
                if hit.is_some() {
                    return;
                }
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let mut p = pointer.clone();
                p.push('/');
                p.push_str(&escape_pointer(k));
                walk(v, p, hit);
                if hit.is_some() {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Convenience wrapper used by the route handlers. Returns `Err` with a
/// human-readable message when the envelope contains a high-entropy leaf.
pub fn reject_high_entropy_in_value(value: &Value) -> Result<(), String> {
    if let Some(hit) = find_high_entropy(value) {
        Err(format!(
            "envelope value at {} looks like a secret \
             (length {}, entropy {:.2} bits/char). \
             Move it into a vault and reference it via secrets_ref.",
            if hit.pointer.is_empty() {
                "<root>"
            } else {
                &hit.pointer
            },
            hit.length,
            hit.entropy,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entropy_of_empty_string_is_zero() {
        assert_eq!(shannon_entropy(""), 0.0);
    }

    #[test]
    fn entropy_of_repeating_char_is_zero() {
        assert!((shannon_entropy("aaaaaaaa") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn entropy_of_natural_english_is_below_threshold() {
        let s = "Build the Stripe integration end-to-end including webhook tests";
        let h = shannon_entropy(s);
        assert!(h < ENTROPY_THRESHOLD, "english entropy {h} should be < {ENTROPY_THRESHOLD}");
    }

    #[test]
    fn entropy_of_random_base64ish_is_above_threshold() {
        // Real-shaped api key: 32 alnum chars, all distinct, distributed.
        let s = "EXAMPLE_4eC39HqLyjWDarjtT1zdp7dc";
        let h = shannon_entropy(s);
        assert!(h > ENTROPY_THRESHOLD, "key entropy {h} should be > {ENTROPY_THRESHOLD}");
    }

    #[test]
    fn detects_high_entropy_string_in_object_value() {
        let v = json!({
            "intent": "Build the integration",
            "secret_field": "abcdefghijklmnopqrstuvwxyz0123456789ABCDE"
        });
        let hit = find_high_entropy(&v).expect("expected hit");
        assert_eq!(hit.pointer, "/secret_field");
    }

    #[test]
    fn ignores_short_high_entropy_strings() {
        // 20-char threshold (need >20).
        let v = json!({"k": "abc123XYZ"});
        assert!(find_high_entropy(&v).is_none());
    }

    #[test]
    fn detects_high_entropy_string_in_nested_array() {
        let v = json!({
            "tags": ["ok", "also-ok", "abcdefghijklmnopqrstuvwxyz0123456789ABCDE"]
        });
        let hit = find_high_entropy(&v).expect("expected hit");
        assert_eq!(hit.pointer, "/tags/2");
    }

    #[test]
    fn natural_envelope_passes() {
        let v = json!({
            "version": 1,
            "intent": "Implement task usage endpoints with budget tracking",
            "prompt_template": "Add migration 003. Wire the routes.",
            "success_criteria": "All tests pass and logs do not include the secret",
            "token_budget": {"input_tokens": 100000, "cost_usd": 5.0}
        });
        assert!(find_high_entropy(&v).is_none());
    }

    #[test]
    fn pointer_escapes_slashes_and_tildes() {
        let mut map = serde_json::Map::new();
        map.insert(
            "weird/name".to_string(),
            Value::String("abcdefghijklmnopqrstuvwxyz0123456789ABCDE".into()),
        );
        let v = Value::Object(map);
        let hit = find_high_entropy(&v).unwrap();
        // / inside a key gets escaped to ~1.
        assert_eq!(hit.pointer, "/weird~1name");
    }

    #[test]
    fn reject_helper_returns_human_message() {
        let v = json!({"k": "abcdefghijklmnopqrstuvwxyz0123456789ABCDE"});
        let err = reject_high_entropy_in_value(&v).unwrap_err();
        assert!(err.contains("/k"));
        assert!(err.contains("secrets_ref"));
    }
}
