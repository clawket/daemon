//! Walk an envelope JSON, find every `secrets_ref` reference, resolve it
//! against the registry, and return the resolved bag.
//!
//! Two reference shapes are supported:
//!
//! 1. String: `"env:ANTHROPIC_API_KEY"` — backend determined by the prefix
//!    before the first colon.
//! 2. Object: `{"backend": "env", "path": "ANTHROPIC_API_KEY", "env": "..."}`
//!    — explicit shape; preferred when the env var name differs from the
//!    vault path.
//!
//! In both shapes the *env-var name* is determined as follows:
//!
//! - object form: `env` field if present, else the leaf key in the JSON
//!   pointer (the field name where the secrets_ref appeared).
//! - string form: the leaf key.
//!
//! References live under a top-level `secrets_ref` key whose value is an
//! object: `{ "API_KEY_NAME": <ref>, ... }`. This keeps the search scope
//! deterministic.

use serde_json::{Map, Value};

use super::{BackendKind, Registry, ResolvedSecrets, SecretError, SecretResult};

#[derive(Debug, Clone)]
pub struct SecretRef {
    pub env_var: String,
    pub backend: BackendKind,
    pub path: String,
}

/// Collect references from `envelope.secrets_ref` without fetching. Errors
/// on any malformed reference so callers see a single clean diagnostic.
pub fn collect_secret_refs(envelope: &Value) -> SecretResult<Vec<SecretRef>> {
    let Some(secrets) = envelope.get("secrets_ref") else {
        return Ok(Vec::new());
    };
    let Some(map) = secrets.as_object() else {
        return Err(SecretError::InvalidReference(
            "envelope.secrets_ref must be an object mapping env-var name → reference".into(),
        ));
    };
    let mut out = Vec::with_capacity(map.len());
    for (env_var, raw) in map {
        out.push(parse_ref(env_var, raw)?);
    }
    Ok(out)
}

fn parse_ref(env_var: &str, raw: &Value) -> SecretResult<SecretRef> {
    if env_var.is_empty() {
        return Err(SecretError::InvalidReference(
            "secrets_ref env-var name cannot be empty".into(),
        ));
    }
    match raw {
        Value::String(s) => parse_string_ref(env_var, s),
        Value::Object(o) => parse_object_ref(env_var, o),
        _ => Err(SecretError::InvalidReference(format!(
            "secrets_ref[{env_var}] must be a string or object"
        ))),
    }
}

fn parse_string_ref(env_var: &str, raw: &str) -> SecretResult<SecretRef> {
    let (scheme, path) = raw.split_once(':').ok_or_else(|| {
        SecretError::InvalidReference(format!(
            "secrets_ref[{env_var}] string must be 'backend:path'"
        ))
    })?;
    if path.is_empty() {
        return Err(SecretError::InvalidReference(format!(
            "secrets_ref[{env_var}] has empty path"
        )));
    }
    Ok(SecretRef {
        env_var: env_var.to_string(),
        backend: BackendKind::from_scheme(scheme)?,
        path: path.to_string(),
    })
}

fn parse_object_ref(env_var: &str, raw: &Map<String, Value>) -> SecretResult<SecretRef> {
    let backend_str = raw
        .get("backend")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SecretError::InvalidReference(format!(
                "secrets_ref[{env_var}] object missing 'backend'"
            ))
        })?;
    let path = raw.get("path").and_then(Value::as_str).ok_or_else(|| {
        SecretError::InvalidReference(format!(
            "secrets_ref[{env_var}] object missing 'path'"
        ))
    })?;
    if path.is_empty() {
        return Err(SecretError::InvalidReference(format!(
            "secrets_ref[{env_var}] has empty path"
        )));
    }
    let env_override = raw
        .get("env")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(env_var);
    Ok(SecretRef {
        env_var: env_override.to_string(),
        backend: BackendKind::from_scheme(backend_str)?,
        path: path.to_string(),
    })
}

/// Fetch every collected reference and build a `ResolvedSecrets` bag. The
/// caller injects the bag into the `execute_task` child process env.
pub fn resolve_all(envelope: &Value, registry: &Registry) -> SecretResult<ResolvedSecrets> {
    let refs = collect_secret_refs(envelope)?;
    let mut bag = ResolvedSecrets::new();
    for r in refs {
        let secret = registry.fetch(r.backend, &r.path)?;
        bag.insert(r.env_var, secret);
    }
    Ok(bag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::keyring::{KeyringBackend, MapKeyringStore};
    use crate::secrets::EnvBackend;
    use serde_json::json;
    use std::sync::Arc;

    fn registry_with_seeded_keyring() -> Registry {
        let store = MapKeyringStore::new();
        store.set("clawket/anthropic", "kr-secret");
        let mut r = Registry::empty();
        r.set(BackendKind::Env, Arc::new(EnvBackend::new()));
        r.set(BackendKind::Keyring, Arc::new(KeyringBackend::new(Box::new(store))));
        r
    }

    #[test]
    fn missing_secrets_ref_resolves_to_empty() {
        let env = json!({"version": 1, "intent": "test"});
        let bag = resolve_all(&env, &Registry::empty()).unwrap();
        assert!(bag.is_empty());
    }

    #[test]
    fn string_ref_resolves_via_env_backend() {
        let key = format!("CLAWKET_TEST_RESOLVE_{}", std::process::id());
        std::env::set_var(&key, "live-key");
        let env = json!({
            "secrets_ref": {
                "ANTHROPIC_API_KEY": format!("env:{key}")
            }
        });
        let bag = resolve_all(&env, &Registry::daemon_default()).unwrap();
        assert_eq!(bag.len(), 1);
        assert_eq!(
            bag.get("ANTHROPIC_API_KEY").unwrap().expose_secret(),
            "live-key"
        );
        std::env::remove_var(&key);
    }

    #[test]
    fn object_ref_supports_env_alias_override() {
        let env = json!({
            "secrets_ref": {
                "MY_INTERNAL_NAME": {
                    "backend": "keyring",
                    "path": "clawket/anthropic",
                    "env": "ANTHROPIC_API_KEY"
                }
            }
        });
        let bag = resolve_all(&env, &registry_with_seeded_keyring()).unwrap();
        assert_eq!(bag.len(), 1);
        assert!(bag.get("MY_INTERNAL_NAME").is_none());
        assert_eq!(
            bag.get("ANTHROPIC_API_KEY").unwrap().expose_secret(),
            "kr-secret"
        );
    }

    #[test]
    fn object_ref_uses_key_when_env_alias_omitted() {
        let env = json!({
            "secrets_ref": {
                "ANTHROPIC_API_KEY": {
                    "backend": "keyring",
                    "path": "clawket/anthropic"
                }
            }
        });
        let bag = resolve_all(&env, &registry_with_seeded_keyring()).unwrap();
        assert_eq!(
            bag.get("ANTHROPIC_API_KEY").unwrap().expose_secret(),
            "kr-secret"
        );
    }

    #[test]
    fn rejects_string_ref_without_colon() {
        let env = json!({"secrets_ref": {"K": "no-colon"}});
        let err = collect_secret_refs(&env).unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }

    #[test]
    fn rejects_unknown_backend() {
        let env = json!({"secrets_ref": {"K": "vault:secret/data/foo"}});
        let err = collect_secret_refs(&env).unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }

    #[test]
    fn rejects_object_without_backend_field() {
        let env = json!({"secrets_ref": {"K": {"path": "p"}}});
        let err = collect_secret_refs(&env).unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }

    #[test]
    fn prompt_backend_resolution_rejected_by_daemon() {
        let env = json!({"secrets_ref": {"K": "prompt:foo"}});
        let err = resolve_all(&env, &Registry::daemon_default()).unwrap_err();
        assert!(matches!(err, SecretError::PromptRejectedInDaemon));
    }

    #[test]
    fn malformed_secrets_ref_block_is_invalid() {
        let env = json!({"secrets_ref": "not-an-object"});
        let err = collect_secret_refs(&env).unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }
}
