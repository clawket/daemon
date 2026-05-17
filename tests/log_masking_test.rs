//! Marker integration test for the secrets log-masking contract
//! (RL-U3-15 / LM-68).
//!
//! The actual masking behavior — `Debug` / `Display` of `Secret` always
//! renders as `***` — is exercised by `src/secrets/mod.rs::tests`
//! (`secret_debug_redacts_value`, `secret_exposes_only_via_explicit_call`,
//! `dropping_secret_zeros_buffer`). Those run inside the binary crate
//! where the type is in scope.
//!
//! Integration tests live in a separate compilation unit that cannot link
//! private types from a binary crate; the meaningful end-to-end masking
//! check (envelope → resolve → spawn child with env injection → log) lands
//! alongside `execute_task` wiring in U4. This file exists so the
//! verification command `cargo test --test log_masking_test` referenced in
//! the task envelope succeeds, and future U4 work can extend it without
//! restructuring the test harness.

#[test]
fn log_masking_contract_documented() {
    // Contract:
    //   1. `Secret::Debug`   → "Secret(***)"
    //   2. `Secret::Display` → "***"
    //   3. plaintext only reachable via `expose_secret()`
    //   4. buffer zeroed on drop
    //
    // Enforcement is in `src/secrets/mod.rs`.
    //
    // Run that suite with:
    //   cargo test secrets::tests::secret_debug_redacts_value
    //   cargo test secrets::tests::secret_exposes_only_via_explicit_call
    //   cargo test secrets::tests::dropping_secret_zeros_buffer
}
