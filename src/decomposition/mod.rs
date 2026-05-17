//! LM-87 / RL-U6-04 — daemon-side decomposition suggestions.
//!
//! The CLI MCP tool `clawket_decompose_task` and the web Suggestion
//! Panel (`features/decomposition/SuggestionPanel.tsx`) both consume
//! this module via `POST /tasks/:id/decompose` so the rule set can't
//! diverge between surfaces. Mirrors the LM-151 validator pattern:
//! daemon owns the rules, MCP and web are thin clients.

pub mod suggest;
