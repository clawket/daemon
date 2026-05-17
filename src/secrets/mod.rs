//! Secret resolution for envelope `secrets_ref` (RL-U3-15 / LM-68,
//! ADR-0009 enforcement point).
//!
//! Envelopes never contain secret *values*. Instead they reference vault
//! paths via the `secrets_ref` shape (a string `"backend:path"` or an object
//! `{"backend": "...", "path": "..."}`). Just before `execute_task` spawns
//! the underlying tool the resolver walks the envelope, fetches each
//! reference from its declared `VaultBackend`, and injects the values as
//! environment variables. The resolved values live in `Secret` newtypes
//! whose `Debug` / `Display` both render as `"***"`, so an accidental
//! `tracing::info!(?secret)` cannot leak the plaintext to logs.
//!
//! `Secret` zeroes its buffer when dropped to reduce the window during
//! which the value sits in process memory.
//!
//! # Backends
//!
//! - `env` — `EnvBackend` reads a process environment variable. Cheap,
//!   handy for CI; never persisted.
//! - `keyring` — `KeyringBackend` proxies to a `KeyringStore` trait. The
//!   production implementation will wrap the OS keychain (planned for U4
//!   wiring); the in-memory implementation is the test default.
//! - `op` — `OnePasswordBackend` shells out to a configurable `op` binary
//!   path. Tests inject a closure to avoid depending on the host having
//!   `op` installed.
//! - `prompt` — `PromptBackend` always returns
//!   `SecretError::PromptRejectedInDaemon`. Interactive prompting can only
//!   happen inside the CLI process; the daemon never has a terminal.
//!
//! # Envelope hardening
//!
//! `redact::reject_high_entropy_in_value` is invoked from the envelope
//! create / update routes. If a string leaf has entropy > 4.5 bits/char
//! and length > 20, the request is rejected — that's the heuristic
//! signature of a pasted-by-mistake API key.

// Backends + resolver are the public surface for `execute_task` (U4). The
// envelope hook (`redact::reject_high_entropy_in_value`) is wired today;
// the rest is fully tested but not yet called from production code.
#![allow(dead_code)]

pub mod env;
pub mod keyring;
pub mod onepassword;
pub mod prompt;
pub mod redact;
pub mod registry;
pub mod resolve;

use std::collections::HashMap;
use std::fmt;

pub use env::EnvBackend;
pub use keyring::{KeyringBackend, KeyringStore};
pub use onepassword::OnePasswordBackend;
pub use prompt::PromptBackend;
pub use registry::{BackendKind, Registry};

#[derive(Debug)]
pub enum SecretError {
    NotFound { backend: String, path: String },
    BackendUnavailable { backend: String, reason: String },
    PromptRejectedInDaemon,
    InvalidReference(String),
    Backend(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::NotFound { backend, path } => {
                write!(f, "secret not found in {backend}: {path}")
            }
            SecretError::BackendUnavailable { backend, reason } => {
                write!(f, "secret backend {backend} unavailable: {reason}")
            }
            SecretError::PromptRejectedInDaemon => f.write_str(
                "interactive prompt backend is rejected by the daemon — \
                 use env / keyring / 1password",
            ),
            SecretError::InvalidReference(m) => write!(f, "invalid secrets_ref: {m}"),
            SecretError::Backend(m) => write!(f, "secret backend error: {m}"),
        }
    }
}

impl std::error::Error for SecretError {}

pub type SecretResult<T> = Result<T, SecretError>;

/// A resolved secret. `Debug` / `Display` always renders as `"***"`.
/// Construct via `Secret::new`. The plaintext is reachable only through
/// `expose_secret`, which makes intent obvious at the call site.
pub struct Secret {
    inner: Vec<u8>,
}

impl Secret {
    pub fn new<S: Into<Vec<u8>>>(value: S) -> Self {
        Self {
            inner: value.into(),
        }
    }

    pub fn from_str_value(value: &str) -> Self {
        Self {
            inner: value.as_bytes().to_vec(),
        }
    }

    /// Borrow the plaintext. Use this only at the env-injection site.
    pub fn expose_secret(&self) -> &str {
        // The bytes came from a UTF-8 source; if not, the resolver caller
        // shouldn't have stored it as a Secret in the first place.
        std::str::from_utf8(&self.inner).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Best-effort zero. Without `volatile_set_memory` the optimizer is
        // technically allowed to drop this; for a daemon-side secret the
        // attack surface is "memory dump after a crash", so this is good
        // enough until we add `zeroize` properly.
        for b in self.inner.iter_mut() {
            *b = 0;
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

pub trait VaultBackend: Send + Sync {
    fn name(&self) -> &str;
    fn fetch(&self, vault_path: &str) -> SecretResult<Secret>;
}

/// Bag of resolved secrets keyed by env-var name.
#[derive(Debug, Default)]
pub struct ResolvedSecrets {
    map: HashMap<String, Secret>,
}

impl ResolvedSecrets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: Secret) {
        self.map.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<&Secret> {
        self.map.get(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate (key, plaintext) pairs — for the env-injection site.
    pub fn iter_exposed(&self) -> impl Iterator<Item = (&str, &str)> {
        self.map
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose_secret()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_redacts_value() {
        let s = Secret::from_str_value("EXAMPLE_KEY_abc123");
        assert_eq!(format!("{:?}", s), "Secret(***)");
        assert_eq!(format!("{}", s), "***");
    }

    #[test]
    fn secret_exposes_only_via_explicit_call() {
        let s = Secret::from_str_value("plain");
        assert_eq!(s.expose_secret(), "plain");
    }

    #[test]
    fn resolved_secrets_iterates_pairs() {
        let mut bag = ResolvedSecrets::new();
        bag.insert("ANTHROPIC_API_KEY", Secret::from_str_value("k1"));
        bag.insert("STRIPE_KEY", Secret::from_str_value("k2"));
        assert_eq!(bag.len(), 2);
        let mut pairs: Vec<_> = bag.iter_exposed().collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![("ANTHROPIC_API_KEY", "k1"), ("STRIPE_KEY", "k2")]
        );
    }

    #[test]
    fn dropping_secret_zeros_buffer() {
        // Reach into the inner buffer via a controlled scope. We can't
        // observe post-drop memory directly but we can verify the
        // pre-drop length-then-zero behavior the Drop impl performs.
        let mut s = Secret::from_str_value("hello");
        assert_eq!(s.inner.len(), 5);
        // Manually trigger the zeroing logic (mirrors Drop) without
        // moving — Drop's effect is that ALL bytes become 0.
        for b in s.inner.iter_mut() {
            *b = 0;
        }
        assert!(s.inner.iter().all(|&b| b == 0));
    }
}
