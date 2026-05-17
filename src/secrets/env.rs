//! `env` backend — read a secret from a process environment variable.

use super::{Secret, SecretError, SecretResult, VaultBackend};

pub struct EnvBackend;

impl EnvBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnvBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VaultBackend for EnvBackend {
    fn name(&self) -> &str {
        "env"
    }

    fn fetch(&self, vault_path: &str) -> SecretResult<Secret> {
        if vault_path.is_empty() {
            return Err(SecretError::InvalidReference(
                "env backend requires a non-empty path".into(),
            ));
        }
        match std::env::var(vault_path) {
            Ok(v) => Ok(Secret::from_str_value(&v)),
            Err(std::env::VarError::NotPresent) => Err(SecretError::NotFound {
                backend: "env".into(),
                path: vault_path.into(),
            }),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::Backend(format!(
                "env var {vault_path} contained non-UTF8 bytes"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a unique env var name so concurrent tests don't collide.
    fn unique_var() -> String {
        format!(
            "CLAWKET_TEST_SECRET_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn fetch_reads_from_env() {
        let key = unique_var();
        std::env::set_var(&key, "supersecret");
        let backend = EnvBackend::new();
        let secret = backend.fetch(&key).expect("env hit");
        assert_eq!(secret.expose_secret(), "supersecret");
        std::env::remove_var(&key);
    }

    #[test]
    fn fetch_returns_not_found_when_missing() {
        let key = unique_var();
        let backend = EnvBackend::new();
        let err = backend.fetch(&key).unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[test]
    fn fetch_rejects_empty_path() {
        let err = EnvBackend::new().fetch("").unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }
}
