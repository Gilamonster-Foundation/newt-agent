//! A shared RAII guard that serializes tests touching the **process-global
//! operator settings** — cognition, tenacity, initiative, and the env vars the psyche /
//! backend-routing paths read — and restores them on drop.
//!
//! These settings are process-wide (`set_cli_cognition` / `set_cli_tenacity` /
//! `set_initiative_config` / `set_active_model_family` / `NEWT_PROVIDER` / …), so
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
//! `effective_tenacity()` / `effective_initiative()` / `effective_cognition()`
//! return between tests. Rather than reach into each global piecemeal, it
//! composes the crate-owned runtime snapshots, which together cover every
//! resolution global:
//!
//! - [`cognition::CognitionRuntimeSnapshot`]: `CLI_COGNITION`, `PERSONA_COGNITION`
//!   (the only two globals read by `effective_cognition`).
//! - [`tenacity::TenacityRuntimeSnapshot`]: `CLI_TENACITY` (the only input to
//!   `effective_tenacity`) and the two round-cap globals.
//! - [`initiative::InitiativeRuntimeSnapshot`]: `CLI_INITIATIVE`,
//!   `PERSONA_INITIATIVE`, **`INITIATIVE_CONFIG`**, **`ACTIVE_FAMILY`** (all four
//!   read by `effective_initiative`). The last two were the gap the old
//!   piecemeal guard missed: `Config::publish_runtime_settings` installs the
//!   config and the `solve` model-selection path installs the active family,
//!   both process-wide, so a test exercising either leaked a per-family default
//!   into a sibling test.
//! - [`crate::runtime::PreferenceRuntimeSnapshot`] (#1668): the posture-ACTION
//!   accumulator and the recorded CLI posture axes. Neither feeds
//!   `effective_*`, but both are process-global operator state written by the
//!   same commands: an action marked by one test and never drained would be
//!   attributed to the NEXT test's conversation, and a recorded CLI axis would
//!   silently suppress another test's pin apply.
//! - [`crate::posture::ActivePosture`]: the resolved permission clamp and
//!   guidance shared by `/posture`, settings, and accepted turns. This is
//!   distinct from the preference-action accumulator above.
//!
//! Plus the env vars below, which are *upstream* (model / backend selection →
//! `ACTIVE_FAMILY`) or *downstream* (cognition wire emission) of the resolutions
//! — not read inside them, but mutated by backend / psyche / crew routing tests.
//! (There is no `NEWT_TENACITY` / `NEWT_INITIATIVE` / `NEWT_COGNITION` env var —
//! the dials are only ever sourced from CLI flags into the globals above.)
//! `CLI_BACKEND_OVERRIDE` (config.rs) is on the backend axis, not read by either
//! resolution fn, so it is intentionally out of scope here.

use crate::cognition::CognitionRuntimeSnapshot;
use crate::initiative::InitiativeRuntimeSnapshot;
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
    // #2665: the bare-install fallback backend's endpoint.
    "OLLAMA_HOST",
    // **Every env-backed `/settings` field.** The guard's own doc says it
    // snapshots "the relevant env", and these are the most relevant there is:
    // a test that flips a form field left the variable set for whatever ran
    // next on this thread. Three of them (#2009 PR4 found this while adding
    // the fourth) had been absent since the fields landed.
    "NEWT_EDIT_MODE",
    "NEWT_THINKING",
    "NEWT_NUDGE",
    "NEWT_MARKDOWN",
    "NEWT_COMPACTION_TRIGGER",
    "NEWT_SPILL_LINES",
    crate::settings_receipt::RECEIPT_PATH_ENV,
    // #2085 PR-E2: the same hazard, one journal later. Six mutators now append
    // to the chained event journal, so a test that runs one would extend the
    // developer's real `~/.newt/events.jsonl` — and, worse than the receipt
    // case, would advance its head ref and chain a test's noise into a real
    // audit trail.
    crate::event_journal::JOURNAL_PATH_ENV,
];

/// How long a test may wait on an event before the test is declared hung.
///
/// This is a HANG GUARD, not a budget, and the distinction is the whole rule
/// (the operator's, 2026-10-01: wall-clock decides nothing diagnostic). A test
/// completes when the event it awaits arrives — a oneshot, a channel, a socket
/// closing — never when a clock says so. The guard exists so a broken arm that
/// never produces its event fails instead of wedging the suite, and it is set
/// where no machine, however loaded, reaches it: measured on 2026-10-01, a
/// 32-thread nextest run beside a build stretched one-second tests past a
/// minute and broke every 30 s "budget" in the unit tier.
///
/// Never combine it with `tokio::time::pause()` while the awaited event comes
/// from another OS thread or a socket: a paused clock auto-advances when the
/// runtime idles and would fire the guard at once.
pub const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(600);

/// Await `fut`, panicking (naming `what`) if it is still pending after
/// [`HANG_GUARD`]. The assertion is the awaited event; this only names a hang.
///
/// `fut` is boxed here, synchronously, before any future of this function's
/// own exists: the guarded future is often a whole agentic turn, and holding
/// one inline in a wrapper's state overflowed a test thread's 2 MiB stack
/// (`ollama_completed_call_survives_cancelled_sibling`, 2026-10-01).
pub fn hang_guarded<F: std::future::Future>(
    what: &str,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    let what = what.to_owned();
    let guarded = tokio::time::timeout(HANG_GUARD, Box::pin(fut));
    async move {
        match guarded.await {
            Ok(out) => out,
            Err(_) => panic!(
                "hung: {what} produced no result within the {}s hang guard",
                HANG_GUARD.as_secs()
            ),
        }
    }
}

/// The blocking twin of [`hang_guarded`] for a `std::sync::mpsc` receiver: the
/// sent value is the event, and only a hang is named.
pub fn recv_guarded<T>(rx: &std::sync::mpsc::Receiver<T>, what: &str) -> T {
    match rx.recv_timeout(HANG_GUARD) {
        Ok(value) => value,
        Err(error) => panic!(
            "hung: {what} produced no result within the {}s hang guard ({error})",
            HANG_GUARD.as_secs()
        ),
    }
}

/// A kernel session config for the unit tier, with the navigation elapsed-time
/// budget out of reach.
///
/// The kernel charges REAL elapsed time — its own `Instant` around every
/// catalog, projection and re-read, plus the harness's auxiliary timers —
/// against `max_elapsed_ms`, 30 s by default, and refuses navigation past it.
/// That budget bounds a slow auxiliary model in production; in a unit test the
/// auxiliary is an instant closure and the elapsed time is the loaded test
/// box. Measured on 2026-10-01 under a 32-thread nextest run beside a build:
/// five `smart_harness` navigation tests failed at 36–100 s with the budget
/// error, none of them about the budget. The budget's own behaviour is tested
/// in agent-harness against its injected clock, where it belongs.
pub fn unbudgeted_session_config() -> agent_harness::SessionConfig {
    agent_harness::SessionConfig {
        max_elapsed_ms: u64::MAX,
        ..Default::default()
    }
}

/// Exclusive access to the process-global operator settings for the duration of a
/// test. Snapshots cognition + tenacity + initiative + the relevant env on `acquire`, restores
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
    initiative: Option<InitiativeRuntimeSnapshot>,
    posture: Option<PreferenceRuntimeSnapshot>,
    permission_posture: Option<crate::posture::ActivePosture>,
    env: Vec<(&'static str, Option<String>)>,
}

impl GlobalSettingsGuard {
    /// Acquire the guard, snapshotting the current settings.
    ///
    /// It also turns BOTH durable journals OFF for the duration. A setting
    /// change is a durable write (#1981) and six state mutators now append to
    /// the chained event journal (#2085), so a test that flips a dial or runs a
    /// mutator would append to the developer's real `~/.newt/receipts.jsonl` or
    /// `~/.newt/events.jsonl` — which it did, once, before the first of these
    /// lines existed. A test that wants to inspect either points
    /// [`crate::settings_receipt::RECEIPT_PATH_ENV`] or
    /// [`crate::event_journal::JOURNAL_PATH_ENV`] at its own file; the default
    /// is silence.
    #[must_use]
    pub fn acquire() -> Self {
        let lock = crate::process_env::lock();
        let env: Vec<(&'static str, Option<String>)> = ENV_KEYS
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        crate::process_env::set_var(crate::settings_receipt::RECEIPT_PATH_ENV, "");
        crate::process_env::set_var(crate::event_journal::JOURNAL_PATH_ENV, "");
        Self {
            _lock: lock,
            cognition: Some(crate::cognition::snapshot_runtime_state()),
            tenacity: Some(crate::tenacity::snapshot_runtime_state()),
            initiative: Some(crate::initiative::snapshot_runtime_state()),
            posture: Some(crate::runtime::snapshot_runtime_state()),
            permission_posture: crate::posture::active_posture(),
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
        if let Some(snap) = self.initiative.take() {
            crate::initiative::restore_runtime_state(snap);
        }
        if let Some(snap) = self.posture.take() {
            crate::runtime::restore_runtime_state(snap);
        }
        crate::posture::set_active_posture(self.permission_posture.take());
        for (k, v) in &self.env {
            // Still under this guard's own lock (it drops after us), and
            // `set_or_remove` re-takes it reentrantly on this same thread.
            crate::process_env::set_or_remove(k, v.as_deref());
        }
    }
}
