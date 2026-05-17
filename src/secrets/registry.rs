//! Backend registry — selects a `VaultBackend` by scheme name.

use std::collections::HashMap;
use std::sync::Arc;

use super::{
    EnvBackend, KeyringBackend, OnePasswordBackend, PromptBackend, Secret, SecretError,
    SecretResult, VaultBackend,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Env,
    Keyring,
    Op,
    Prompt,
}

impl BackendKind {
    pub fn from_scheme(scheme: &str) -> SecretResult<Self> {
        match scheme {
            "env" => Ok(BackendKind::Env),
            "keyring" => Ok(BackendKind::Keyring),
            "op" | "1password" => Ok(BackendKind::Op),
            "prompt" => Ok(BackendKind::Prompt),
            other => Err(SecretError::InvalidReference(format!(
                "unknown secrets backend: {other}"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BackendKind::Env => "env",
            BackendKind::Keyring => "keyring",
            BackendKind::Op => "op",
            BackendKind::Prompt => "prompt",
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    backends: HashMap<BackendKind, Arc<dyn VaultBackend>>,
}

impl Registry {
    /// Empty registry — explicit `with_*` calls add backends.
    pub fn empty() -> Self {
        Self {
            backends: HashMap::new(),
        }
    }

    /// Daemon-default registry: env + keyring (in-memory placeholder) + op
    /// (shellout) + prompt (always rejects). Production wiring overrides
    /// the keyring + op backends with real implementations.
    pub fn daemon_default() -> Self {
        let mut r = Self::empty();
        r.set(BackendKind::Env, Arc::new(EnvBackend::new()));
        r.set(BackendKind::Keyring, Arc::new(KeyringBackend::in_memory()));
        r.set(BackendKind::Op, Arc::new(OnePasswordBackend::shellout()));
        r.set(BackendKind::Prompt, Arc::new(PromptBackend::new()));
        r
    }

    pub fn set(&mut self, kind: BackendKind, backend: Arc<dyn VaultBackend>) {
        self.backends.insert(kind, backend);
    }

    pub fn fetch(&self, kind: BackendKind, path: &str) -> SecretResult<Secret> {
        let backend = self.backends.get(&kind).ok_or_else(|| {
            SecretError::BackendUnavailable {
                backend: kind.as_str().into(),
                reason: "backend not registered".into(),
            }
        })?;
        backend.fetch(path)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::daemon_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_scheme_recognizes_all_known_schemes() {
        assert_eq!(BackendKind::from_scheme("env").unwrap(), BackendKind::Env);
        assert_eq!(
            BackendKind::from_scheme("keyring").unwrap(),
            BackendKind::Keyring
        );
        assert_eq!(BackendKind::from_scheme("op").unwrap(), BackendKind::Op);
        assert_eq!(
            BackendKind::from_scheme("1password").unwrap(),
            BackendKind::Op
        );
        assert_eq!(
            BackendKind::from_scheme("prompt").unwrap(),
            BackendKind::Prompt
        );
    }

    #[test]
    fn from_scheme_rejects_unknown() {
        let err = BackendKind::from_scheme("vault").unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }

    #[test]
    fn empty_registry_reports_backend_unavailable() {
        let r = Registry::empty();
        let err = r.fetch(BackendKind::Env, "ANY").unwrap_err();
        assert!(matches!(err, SecretError::BackendUnavailable { .. }));
    }

    #[test]
    fn daemon_default_includes_all_four_backends() {
        let r = Registry::daemon_default();
        // env without the var → NotFound (proves backend is registered).
        let err = r
            .fetch(BackendKind::Env, "CLAWKET_DEFINITELY_UNSET_VAR_xyz")
            .unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
        // keyring without entry → NotFound.
        let err = r.fetch(BackendKind::Keyring, "missing").unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
        // prompt → always rejected by the daemon contract.
        let err = r.fetch(BackendKind::Prompt, "any").unwrap_err();
        assert!(matches!(err, SecretError::PromptRejectedInDaemon));
    }
}
