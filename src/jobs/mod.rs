//! Background daemon jobs.
//!
//! Currently houses the activity_log retention rollup (ADR-0010 / RL-U3-16).
//! Scheduled jobs live alongside the public router so they can share the
//! `AppState` connection without re-resolving paths.

pub mod activity_log_rollup;
