//! A shared RAII guard that serializes tests touching the **process-global
//! operator settings** — cognition, tenacity, and the env vars the psyche /
//! backend-routing paths read — and restores them on drop.
//!
//! These settings are process-wide (`set_cli_cognition` / `set_cli_tenacity` /
//! `set_tenacity_config` / `set_active_model_family` / `NEWT_PROVIDER` / …), so
//! tests in *different modules* that mutate them will interleave under the test
//! runner's threads, and manual end-of-test restoration does not survive a panic,
//! an early return, or an assertion failure. One shared lock + a Drop-restored
//! snapshot fixes both: acquire the guard at the top of any test that reads or
//! writes these globals.
//!
//! ```ignore
//! let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
//! // …mutate cognition / tenacity / config / family / NEWT_* freely; restored…
//! ```
//!
//! Exposed (not `#[cfg(test)]`) so tests in dependent crates (`newt-tui`) share
//! the SAME lock — a module-local mutex cannot serialize tests in other crates.
//!
//! ## What the snapshot covers, and why (audited 2026-07-31)
//!
//! The guard must snapshot **exactly** the mutable state that can change what
//! `effective_tenacity()` / `effective_cognition()` return between tests. Rather
//! than reach into each global piecemeal, it composes the two crate-owned runtime
//! snapshots, which together cover all six resolution globals:
//!
//! - [`cognition::CognitionRuntimeSnapshot`]: `CLI_COGNITION`, `PERSONA_COGNITION`
//!   (the only two globals read by `effective_cognition`).
//! - [`tenacity::TenacityRuntimeSnapshot`]: `CLI_TENACITY`, `PERSONA_TENACITY`,
//!   **`TENACITY_CONFIG`**, **`ACTIVE_FAMILY`** (all four read by
//!   `effective_tenacity`). The last two were the gap the piecemeal guard missed:
//!   `Config::apply_runtime_settings` installs the `[tenacity]` config and the `solve`
//!   model-selection path installs the active family, both process-wide, so a
//!   test exercising either leaked a per-family default / active family into a
//!   sibling test's `effective_tenacity()`.
//!
//! Plus the env vars below, which are *upstream* (model / backend selection →
//! `ACTIVE_FAMILY`) or *downstream* (cognition wire emission) of the resolutions
//! — not read inside them, but mutated by backend / psyche / crew routing tests.
//! (There is no `NEWT_TENACITY` / `NEWT_COGNITION` env var — tenacity and
//! cognition are only ever sourced from CLI flags into the globals above.)
//! `CLI_BACKEND_OVERRIDE` (config.rs) is on the backend axis, not read by either
//! resolution fn, so it is intentionally out of scope here.

use crate::cognition::CognitionRuntimeSnapshot;
use crate::tenacity::TenacityRuntimeSnapshot;
use std::sync::{Mutex, MutexGuard};

/// The one lock that serializes every test touching the operator globals.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

/// The env vars the psyche + backend-routing paths read (and tests mutate).
/// `NEWT_OPENAI_API` gates the cognition wire scope, so it belongs here too.
const ENV_KEYS: &[&str] = &[
    "NEWT_TEAM",
    "NEWT_PROVIDER",
    "NEWT_DGX_MODEL",
    "NEWT_OPENAI_API",
];

/// Exclusive access to the process-global operator settings for the duration of a
/// test. Snapshots cognition + tenacity + the relevant env on `acquire`, restores
/// them on `drop` — even through a panic or assertion failure.
#[doc(hidden)]
pub struct GlobalSettingsGuard {
    // Held for the guard's lifetime; a poisoned lock is recovered (a prior test
    // that panicked mid-mutation shouldn't wedge the whole suite).
    _lock: MutexGuard<'static, ()>,
    // `Option` only so `Drop` can move the snapshot out into the restore fns.
    cognition: Option<CognitionRuntimeSnapshot>,
    tenacity: Option<TenacityRuntimeSnapshot>,
    env: Vec<(&'static str, Option<String>)>,
}

impl GlobalSettingsGuard {
    /// Acquire the guard, snapshotting the current settings.
    #[must_use]
    pub fn acquire() -> Self {
        let lock = SETTINGS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        Self {
            _lock: lock,
            cognition: Some(crate::cognition::snapshot_runtime_state()),
            tenacity: Some(crate::tenacity::snapshot_runtime_state()),
            env,
        }
    }
}

impl Drop for GlobalSettingsGuard {
    fn drop(&mut self) {
        if let Some(snap) = self.cognition.take() {
            crate::cognition::restore_runtime_state(snap);
        }
        if let Some(snap) = self.tenacity.take() {
            crate::tenacity::restore_runtime_state(snap);
        }
        for (k, v) in &self.env {
            // SAFETY: the guard holds the settings lock, so no other guarded test
            // is mutating env concurrently; restoration runs single-threaded here.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
