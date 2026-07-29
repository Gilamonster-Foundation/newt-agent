//! `newt bench` — Terminal-Bench matrix orchestration (#1490).
//!
//! newt owns the **roster, sequencing, scoreboard, and gates**; a pluggable
//! [`executor`] backs the actual task execution (harbor today, a native runner
//! later). The roster is a manifest ([`config::MatrixConfig`]) — a new model is
//! a config entry, never code.
//!
//! Hard rule: **one model at a time.** The model loop is strictly sequential —
//! dgx1's shared unified-memory pool holds a single model, and concurrent loads
//! fail. Suite-level concurrency (`n_concurrent_trials`) applies only within one
//! already-loaded model.

pub mod config;
