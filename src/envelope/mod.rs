//! Envelope-side computations that don't fit cleanly into a single
//! route handler — currently the precondition/postcondition evaluator
//! (RL-U3-10 / LM-63). Routes consume these from `envelope::conditions`.

pub mod conditions;
pub mod validate;
