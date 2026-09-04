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
//! ## The lock lives in [`crate::process_env`] (#1850)
//!
//! This guard used to own a private `Mutex`, and `newt-tui` owned a *second*,
//! independent `RwLock` over the same variables. Two locks over one process
//! environment serialize nothing: a test holding either one raced every test
//! holding the other, which is what made `cargo test -p newt-tui --lib
//! --all-features` fail ~30% of runs with whole modules going down together.
//! Both now delegate to the single reentrant lock in [`crate::process_env`],
//! which the production writers take too.
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
//! - [`crate::runtime::PreferenceRuntimeSnapshot`] (#1668): the posture-ACTION
//!   accumulator and the recorded CLI posture axes. Neither feeds
//!   `effective_*`, but both are process-global operator state written by the
//!   same commands: an action marked by one test and never drained would be
//!   attributed to the NEXT test's conversation, and a recorded CLI axis would
//!   silently suppress another test's pin apply.
//!
//! Plus the env vars below, which are *upstream* (model / backend selection →
//! `ACTIVE_FAMILY`) or *downstream* (cognition wire emission) of the resolutions
//! — not read inside them, but mutated by backend / psyche / crew routing tests.
//! (There is no `NEWT_TENACITY` / `NEWT_COGNITION` env var — tenacity and
//! cognition are only ever sourced from CLI flags into the globals above.)
//! `CLI_BACKEND_OVERRIDE` (config.rs) is on the backend axis, not read by either
//! resolution fn, so it is intentionally out of scope here.

use crate::cognition::CognitionRuntimeSnapshot;
use crate::process_env::EnvGuard;
use crate::runtime::PreferenceRuntimeSnapshot;
use crate::tenacity::TenacityRuntimeSnapshot;

/// The env vars the psyche + backend-routing paths read (and tests mutate).
/// `NEWT_OPENAI_API` gates the cognition wire scope, so it belongs here too.
const ENV_KEYS: &[&str] = &[
    "NEWT_TEAM",
    "NEWT_PROVIDER",
    "NEWT_DGX_MODEL",
    "NEWT_OPENAI_API",
    // **Every env-backed `/settings` field.** The guard's own doc says it
    // snapshots "the relevant env", and these are the most relevant there is:
    // a test that flips a form field left the variable set for whatever ran
    // next on this thread. Three of them (#2009 PR4 found this while adding
    // the fourth) had been absent since the fields landed.
    "NEWT_EDIT_MODE",
    "NEWT_THINKING",
    "NEWT_NUDGE",
    "NEWT_MARKDOWN",
    crate::settings_receipt::RECEIPT_PATH_ENV,
];

/// Exclusive access to the process-global operator settings for the duration of a
/// test. Snapshots cognition + tenacity + the relevant env on `acquire`, restores
/// them on `drop` — even through a panic or assertion failure.
#[doc(hidden)]
pub struct GlobalSettingsGuard {
    // Held for the guard's lifetime. `process_env`'s lock is reentrant and
    // unpoisoned, so a test that panics mid-mutation releases it on unwind
    // instead of wedging the suite.
    _lock: EnvGuard,
    // `Option` only so `Drop` can move the snapshot out into the restore fns.
    cognition: Option<CognitionRuntimeSnapshot>,
    tenacity: Option<TenacityRuntimeSnapshot>,
    posture: Option<PreferenceRuntimeSnapshot>,
    env: Vec<(&'static str, Option<String>)>,
}

impl GlobalSettingsGuard {
    /// Acquire the guard, snapshotting the current settings.
    ///
    /// It also turns the settings-receipt journal OFF for the duration. A
    /// setting change is now a durable write (#1981), so a test that flips a
    /// dial would append to the developer's real `~/.newt/receipts.jsonl` —
    /// which it did, once, before this line existed. A test that wants to
    /// inspect the journal points [`crate::settings_receipt::RECEIPT_PATH_ENV`]
    /// at its own file; the default is silence.
    #[must_use]
    pub fn acquire() -> Self {
        let lock = crate::process_env::lock();
        let env: Vec<(&'static str, Option<String>)> = ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        crate::process_env::set_var(crate::settings_receipt::RECEIPT_PATH_ENV, "");
        Self {
            _lock: lock,
            cognition: Some(crate::cognition::snapshot_runtime_state()),
            tenacity: Some(crate::tenacity::snapshot_runtime_state()),
            posture: Some(crate::runtime::snapshot_runtime_state()),
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
        if let Some(snap) = self.posture.take() {
            crate::runtime::restore_runtime_state(snap);
        }
        for (k, v) in &self.env {
            // Still under this guard's own lock (it drops after us), and
            // `set_or_remove` re-takes it reentrantly on this same thread.
            crate::process_env::set_or_remove(k, v.as_deref());
        }
    }
}
