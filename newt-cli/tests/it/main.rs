//! The crate's integration tests that can share a process. Each file here is a
//! module of one test executable, `it`, so it is linked once instead of once
//! per file. A file that is not declared below is not built, and its tests do
//! not run. The files left in `tests/` stay separate executables because they
//! change the process environment, re-run themselves by test name, or are named
//! by `--test` or `binary_id` in a workflow or the pre-push hook.

#[path = "../common/mod.rs"]
mod common;

mod capability_wiring;
mod cli_tests;
mod config_isolation;
mod doctor_cli;
mod doctor_durable_grants;
mod frame_cli;
mod headless_cli;
mod identity_cli;
mod mcp_probe_cli;
mod net_guard_selfexec_cli;
mod ocap_denials_cli;
mod providers_cli;
mod stale_grant_startup;
mod timer_cli;
mod tunings_cli;
mod web_cli;
mod worker_cli;
