//! `prompt` backend — interactive stdin prompt.
//!
//! The daemon never has a terminal so it always rejects this backend with
//! `PromptRejectedInDaemon`. The CLI process can wire a real interactive
//! prompt later if we ever decide to; the daemon API surface stays clean.

use super::{Secret, SecretError, SecretResult, VaultBackend};

pub struct PromptBackend;

impl PromptBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PromptBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VaultBackend for PromptBackend {
    fn name(&self) -> &str {
        "prompt"
    }

    fn fetch(&self, _vault_path: &str) -> SecretResult<Secret> {
        Err(SecretError::PromptRejectedInDaemon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_always_rejects_prompt_backend() {
        let backend = PromptBackend::new();
        let err = backend.fetch("any/path").unwrap_err();
        assert!(matches!(err, SecretError::PromptRejectedInDaemon));
        // Even an empty path returns the same rejection — the daemon-side
        // contract is "this backend is unusable here", not a path check.
        let err2 = backend.fetch("").unwrap_err();
        assert!(matches!(err2, SecretError::PromptRejectedInDaemon));
    }

    #[test]
    fn name_is_prompt() {
        assert_eq!(PromptBackend::new().name(), "prompt");
    }
}
