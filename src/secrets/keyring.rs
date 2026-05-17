//! `keyring` backend — proxies to a `KeyringStore`.
//!
//! Real OS keychain integration lands when `execute_task` ships in U4. For
//! today's resolver we expose the trait + an in-memory implementation so
//! the structure can be unit-tested deterministically. Production wiring
//! will swap in a `SystemKeyringStore` (likely the `keyring` crate) without
//! touching the resolver.

use std::collections::HashMap;
use std::sync::RwLock;

use super::{Secret, SecretError, SecretResult, VaultBackend};

pub trait KeyringStore: Send + Sync {
    fn get(&self, path: &str) -> SecretResult<String>;
}

/// Test / scaffolding store that holds entries in process memory.
pub struct MapKeyringStore {
    entries: RwLock<HashMap<String, String>>,
}

impl MapKeyringStore {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn set(&self, path: impl Into<String>, value: impl Into<String>) {
        let mut g = self.entries.write().unwrap();
        g.insert(path.into(), value.into());
    }
}

impl Default for MapKeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringStore for MapKeyringStore {
    fn get(&self, path: &str) -> SecretResult<String> {
        let g = self.entries.read().unwrap();
        g.get(path).cloned().ok_or_else(|| SecretError::NotFound {
            backend: "keyring".into(),
            path: path.into(),
        })
    }
}

pub struct KeyringBackend {
    store: Box<dyn KeyringStore>,
}

impl KeyringBackend {
    pub fn new(store: Box<dyn KeyringStore>) -> Self {
        Self { store }
    }

    pub fn in_memory() -> Self {
        Self::new(Box::new(MapKeyringStore::new()))
    }
}

impl VaultBackend for KeyringBackend {
    fn name(&self) -> &str {
        "keyring"
    }

    fn fetch(&self, vault_path: &str) -> SecretResult<Secret> {
        if vault_path.is_empty() {
            return Err(SecretError::InvalidReference(
                "keyring backend requires a non-empty path".into(),
            ));
        }
        let value = self.store.get(vault_path)?;
        Ok(Secret::from_str_value(&value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_store_round_trips_value() {
        let store = MapKeyringStore::new();
        store.set("clawket/anthropic", "v1");
        let backend = KeyringBackend::new(Box::new(store));
        let s = backend.fetch("clawket/anthropic").unwrap();
        assert_eq!(s.expose_secret(), "v1");
    }

    #[test]
    fn missing_entry_returns_not_found() {
        let backend = KeyringBackend::in_memory();
        let err = backend.fetch("missing").unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[test]
    fn empty_path_is_invalid_reference() {
        let backend = KeyringBackend::in_memory();
        let err = backend.fetch("").unwrap_err();
        assert!(matches!(err, SecretError::InvalidReference(_)));
    }
}
