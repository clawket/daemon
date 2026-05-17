// FIX-DAEMON-018: Locale resolution chain.
#![allow(dead_code)]
//
// Resolution order (highest to lowest precedence):
//   1. CLAWKET_LOCALE  — Clawket-specific override
//   2. LC_ALL          — POSIX standard locale override
//   3. LANG            — POSIX default locale
//   4. "en"            — hardcoded fallback
//
// Returned value is a BCP-47 language tag string (e.g. "ko", "en-US", "ja").
// The raw env value may be a POSIX locale string like "ko_KR.UTF-8"; we
// normalise it to a BCP-47 tag by stripping the charset suffix and replacing
// underscore with hyphen.

use std::env;

/// Resolve the daemon's active locale using the documented env chain.
pub fn resolve_locale() -> String {
    let raw = env::var("CLAWKET_LOCALE")
        .or_else(|_| env::var("LC_ALL"))
        .or_else(|_| env::var("LANG"))
        .unwrap_or_else(|_| "en".to_string());
    normalize_locale(&raw)
}

/// Normalise a POSIX locale string to a BCP-47 language tag.
///
/// Examples:
///   "ko_KR.UTF-8" → "ko-KR"
///   "en_US.UTF-8" → "en-US"
///   "C"           → "en"
///   "POSIX"       → "en"
///   "ja"          → "ja"
fn normalize_locale(raw: &str) -> String {
    // Strip charset (e.g. ".UTF-8").
    let without_charset = raw.split('.').next().unwrap_or(raw);
    // Replace underscore with hyphen for BCP-47.
    let tag = without_charset.replace('_', "-");
    // Map POSIX/C pseudo-locales to "en".
    match tag.as_str() {
        "C" | "POSIX" | "" => "en".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_posix_locale() {
        assert_eq!(normalize_locale("ko_KR.UTF-8"), "ko-KR");
        assert_eq!(normalize_locale("en_US.UTF-8"), "en-US");
        assert_eq!(normalize_locale("ja_JP.UTF-8"), "ja-JP");
        assert_eq!(normalize_locale("C"), "en");
        assert_eq!(normalize_locale("POSIX"), "en");
        assert_eq!(normalize_locale("ko"), "ko");
        assert_eq!(normalize_locale(""), "en");
    }
}
