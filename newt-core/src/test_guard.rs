//! A shared RAII guard that serializes tests touching the **process-global
//! operator settings** — cognition, tenacity, and the env vars the psyche /
//! backend-routing paths read — and restores them on drop.
//!
//! These settings are process-wide (`set_cli_cognition` / `set_cli_tenacity` /
//! `NEWT_PROVIDER` / …), so tests in *different modules* that mutate them will
//! interleave under the test runner's threads, and manual end-of-test restoration
//! does not survive a panic, an early return, or an assertion failure. One shared
//! lock + a Drop-restored snapshot fixes both: acquire the guard at the top of any
//! test that reads or writes these globals.
//!
//! ```ignore
//! let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
//! // …mutate cognition / tenacity / NEWT_* freely; restored on drop…
//! ```
//!
//! Exposed (not `#[cfg(test)]`) so tests in dependent crates (`newt-tui`) share
//! the SAME lock — a module-local mutex cannot serialize tests in other crates.

use crate::cognition::{
    cli_cognition, persona_cognition, set_cli_cognition, set_persona_cognition, CognitionOverride,
};
use crate::role_profile::Cognition;
use crate::tenacity::{
    clear_cli_tenacity, cli_tenacity, persona_tenacity, set_cli_tenacity, set_persona_tenacity,
    Tenacity,
};
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
    cognition: CognitionOverride,
    persona_cognition: Option<Cognition>,
    tenacity: Option<Tenacity>,
    persona_tenacity: Option<Tenacity>,
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
            cognition: cli_cognition(),
            persona_cognition: persona_cognition(),
            tenacity: cli_tenacity(),
            persona_tenacity: persona_tenacity(),
            env,
        }
    }
}

impl Drop for GlobalSettingsGuard {
    fn drop(&mut self) {
        set_cli_cognition(self.cognition);
        set_persona_cognition(self.persona_cognition);
        set_persona_tenacity(self.persona_tenacity);
        match self.tenacity {
            Some(t) => set_cli_tenacity(t),
            None => clear_cli_tenacity(),
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
