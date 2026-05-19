//! Cross-cutting infrastructure for outbound calls.
//!
//! Currently exports the rate limiter used by future `execute_task` work
//! (RL-U3-13 / LM-66) — exponential backoff + jitter for 429/529 responses
//! plus a circuit breaker that fails fast under sustained pressure.

/// US-CKT-SCHEMA-037: 503 gate while schema migrations are in flight.
pub mod migration_gate;
pub mod rate_limit;
/// FIX-DAEMON-108: TCP auth middleware (X-Clawket-Token)
pub mod tcp_auth;
