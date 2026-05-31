//! Heuristic detector for accidentally pasted secrets in envelope JSON.
//!
//! Strings whose Shannon entropy exceeds 4.5 bits per character AND whose
//! length exceeds 20 characters look like base64/hex API keys and almost
//! never appear in legitimate prose. The envelope create / update routes
//! reject any such leaf — users must move the value into a vault and
//! reference it through `secrets_ref`.
//!
//! Per-character Shannon entropy alone over-fires on natural language whose
//! alphabet is large — most notably CJK (Korean / Chinese / Japanese), where
//! nearly every character in a sentence is distinct, pushing per-char entropy
//! well past 4.5 even though the text is plain prose. A pasted key/token has a
//! distinct *structural* shape that prose never has: it is a single ASCII token
//! with no whitespace. We therefore only flag a high-entropy leaf when it also
//! looks like a key (see `looks_like_secret`), which exempts prose in any
//! script.

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

/// Structural gate that distinguishes a pasted key/token from natural-language
/// prose, applied before the entropy threshold so prose never trips it.
///
/// A real base64/hex/API key is a single ASCII token: it has no whitespace and
/// no characters outside the ASCII range. Natural language always violates one
/// of those — English/Latin prose carries spaces, and CJK (whose dense
/// per-character entropy is the actual source of the false positive) carries
/// non-ASCII letters. Requiring *both* "no whitespace" and "pure ASCII" keeps
/// the detector firing on actual keys while exempting prose in any script.
fn looks_like_secret(s: &str) -> bool {
    s.is_ascii() && !s.chars().any(|c| c.is_whitespace())
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
            if s.len() > MIN_LENGTH && looks_like_secret(s) {
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
        assert!(
            h < ENTROPY_THRESHOLD,
            "english entropy {h} should be < {ENTROPY_THRESHOLD}"
        );
    }

    #[test]
    fn entropy_of_random_base64ish_is_above_threshold() {
        // Real-shaped api key: 32 alnum chars, all distinct, distributed.
        let s = "EXAMPLE_4eC39HqLyjWDarjtT1zdp7dc";
        let h = shannon_entropy(s);
        assert!(
            h > ENTROPY_THRESHOLD,
            "key entropy {h} should be > {ENTROPY_THRESHOLD}"
        );
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

    #[test]
    fn korean_prose_envelope_passes_despite_high_per_char_entropy() {
        // Regression for #50: plain Korean envelope text has per-char Shannon
        // entropy well above 4.5 (nearly every syllable is distinct) but is not
        // a secret. It must pass.
        let intent =
            "데몬 라우트 핸들러에서 시나리오 식별자 검증 로직을 재구성하고 회귀 테스트를 추가한다";
        assert!(
            shannon_entropy(intent) > ENTROPY_THRESHOLD,
            "precondition: korean prose entropy should exceed the threshold"
        );
        let v = json!({
            "intent": intent,
            "prompt_template": "주어진 시나리오를 만족하도록 데몬 라우트를 구현하세요.",
            "success_criteria": "한국어 envelope 로 태스크가 secret 오탐 없이 생성된다"
        });
        assert!(
            find_high_entropy(&v).is_none(),
            "korean prose must not be flagged as a secret"
        );
    }

    #[test]
    fn high_entropy_english_with_whitespace_passes() {
        // A long, information-dense English sentence with spaces is prose, not
        // a key — whitespace alone exempts it even if entropy is borderline.
        let v = json!({
            "intent": "Migrate the linear gate graph into a reentrant cycle graph abstraction"
        });
        assert!(find_high_entropy(&v).is_none());
    }

    #[test]
    fn cjk_without_whitespace_still_passes() {
        // Even a Korean sentence with no spaces is prose (non-ASCII), so the
        // whitespace heuristic is not the only thing exempting CJK.
        let v = json!({"intent": "작업실행계획을수립하고검증가능한단위로분해한다"});
        assert!(find_high_entropy(&v).is_none());
    }

    #[test]
    fn ascii_key_without_whitespace_is_still_flagged() {
        // The structural gate must NOT weaken detection of real pasted keys.
        // (Synthetic high-entropy token with no provider prefix.)
        let v = json!({"token": "Xk7Qm2Zv9Wb4Rt6Yn1Lp3Hs8Dg5Fj0Ca"});
        let hit = find_high_entropy(&v).expect("real key must still be flagged");
        assert_eq!(hit.pointer, "/token");
    }

    #[test]
    fn looks_like_secret_gate() {
        assert!(looks_like_secret("abcDEF0123456789ghijkl"));
        assert!(!looks_like_secret("has a space in it somewhere here"));
        assert!(!looks_like_secret("한글이라서ASCII아님"));
    }
}
