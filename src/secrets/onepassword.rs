//! `op` backend — shells out to the 1Password CLI.
//!
//! Production wiring runs `op read 'op://Vault/Item/Field'`; tests inject a
//! closure to avoid depending on the host having `op` installed (and to
//! keep tests hermetic).

use std::sync::Arc;

use super::{Secret, SecretError, SecretResult, VaultBackend};

pub type OpFetcher = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

pub struct OnePasswordBackend {
    fetcher: OpFetcher,
}

impl OnePasswordBackend {
    pub fn new(fetcher: OpFetcher) -> Self {
        Self { fetcher }
    }

    /// Default fetcher: shell out to `op read <path>`.
    pub fn shellout() -> Self {
        let fetcher: OpFetcher = Arc::new(|path: &str| {
            let output = std::process::Command::new("op")
                .arg("read")
                .arg(path)
                .output()
                .map_err(|e| format!("failed to spawn op: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("op exited with {}: {}", output.status, stderr));
            }
            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_string();
            Ok(stdout)
        });
        Self { fetcher }
    }
}

impl VaultBackend for OnePasswordBackend {
    fn name(&self) -> &str {
        "op"
    }

    fn fetch(&self, vault_path: &str) -> SecretResult<Secret> {
        if vault_path.is_empty() {
            return Err(SecretError::InvalidReference(
                "op backend requires a non-empty path".into(),
            ));
        }
        match (self.fetcher)(vault_path) {
            Ok(v) if v.is_empty() => Err(SecretError::NotFound {
                backend: "op".into(),
                path: vault_path.into(),
            }),
            Ok(v) => Ok(Secret::from_str_value(&v)),
            Err(e) => Err(SecretError::BackendUnavailable {
                backend: "op".into(),
                reason: e,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_returns_secret_from_injected_fetcher() {
        let backend = OnePasswordBackend::new(Arc::new(|p: &str| {
            assert_eq!(p, "op://Vault/Item/Field");
            Ok("from-1pw".to_string())
        }));
        let s = backend.fetch("op://Vault/Item/Field").unwrap();
        assert_eq!(s.expose_secret(), "from-1pw");
    }

    #[test]
    fn fetch_treats_empty_stdout_as_not_found() {
        let backend = OnePasswordBackend::new(Arc::new(|_| Ok(String::new())));
        let err = backend.fetch("any").unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[test]
    fn fetch_propagates_op_failure_as_backend_unavailable() {
        let backend = OnePasswordBackend::new(Arc::new(|_| {
            Err("op exited with 127: command not found".to_string())
        }));
        let err = backend.fetch("any").unwrap_err();
        assert!(matches!(err, SecretError::BackendUnavailable { .. }));
    }

    #[test]
    fn empty_path_is_invalid_reference() {
        let backend = OnePasswordBackend::new(Arc::new(|_| Ok("x".into())));
        let err = backend.fetch("").unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }
}
