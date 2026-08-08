//! Typed, immutable **launch authority** — the OCAP-off / full-access switches
//! resolved **once** near startup and then FROZEN for the process.
//!
//! # Why this exists (`noninteractive-launch-policy` closure)
//!
//! The authority switches (`--disable-ocap`/`--yolo`, `--full-access`,
//! `--unsafe-host-exec`) have env twins (`NEWT_DISABLE_OCAP`,
//! `NEWT_FULL_ACCESS`, `NEWT_UNSAFE_HOST_EXEC`) so a wrapper/pod can assert them.
//! Historically deep libraries decided authority by reading those env vars
//! *live* (`std::env::var(...)`) at the point of use — so a value that appeared
//! **after** startup (an inherited var, or a hostile tool that managed to set
//! one mid-session) could *widen* the running process's authority. That is a
//! confused-deputy: authority is supposed to be a startup decision, not an
//! ambient signal a later actor can flip.
//!
//! This module makes authority a **value**, not an ambient read:
//!
//! - [`LaunchAuthority::from_env`] is the **only** place that reads the three
//!   authority env vars. A widening switch reads fail-closed (the value must be
//!   exactly `"1"`).
//! - [`freeze`] records the resolved authority once, near startup (the CLI
//!   entrypoints call it after they translate flags to env twins). First freeze
//!   wins; a later call can never widen it.
//! - [`current`] returns the frozen value. Deep libraries call this instead of
//!   `std::env::var`, so a later-appearing env var cannot change authority.
//! - The switches are private bits with getters only; the sole combinator
//!   ([`LaunchAuthority::meet`]) can *attenuate* (clear bits), never widen.
//!
//! The ratchet that keeps it honest is a source-inventory gate (`ocap_check.py`):
//! `std::env::var("NEWT_DISABLE_OCAP" | "NEWT_FULL_ACCESS" | "NEWT_UNSAFE_HOST_EXEC")`
//! may appear **only** in this file. Any other deep ambient authority read
//! re-opens the deviation.

/// The three ambient authority switches, as one immutable value. `Copy` so it
/// is returned and threaded freely; the bits are private and there is
/// deliberately no setter that *widens* — construction is via [`from_env`] (the
/// startup resolver), the [`CONFINED`] default, or an attenuating [`meet`].
///
/// [`from_env`]: LaunchAuthority::from_env
/// [`CONFINED`]: LaunchAuthority::CONFINED
/// [`meet`]: LaunchAuthority::meet
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchAuthority {
    /// `--disable-ocap` / `--yolo` (`NEWT_DISABLE_OCAP=1`): run `run_command`
    /// UNCONFINED on the host shell instead of the confined engine.
    ocap_disabled: bool,
    /// `--full-access` (`NEWT_FULL_ACCESS=1`): build the session policy from the
    /// `full_access` preset (`Caveats::top()`) regardless of config.
    full_access: bool,
    /// `--unsafe-host-exec` (`NEWT_UNSAFE_HOST_EXEC=1`): the explicit opt-in that
    /// lets a headless lane select the OCAP-off host-exec path.
    unsafe_host_exec: bool,
}

impl LaunchAuthority {
    /// Fully confined — every switch off. The safe default (same as
    /// `LaunchAuthority::default()`), and what a process with no authority flags
    /// resolves to.
    pub const CONFINED: Self = Self {
        ocap_disabled: false,
        full_access: false,
        unsafe_host_exec: false,
    };

    /// Read exactly `"1"` from the env twin — a widening switch is fail-closed,
    /// so anything else (including `"true"`) leaves it off.
    fn env_switch(key: &str) -> bool {
        std::env::var(key).is_ok_and(|v| v == "1")
    }

    /// Resolve the launch authority from the process environment. **This is the
    /// only function in the workspace that reads the authority env vars**; every
    /// other consumer goes through [`current`]. Callers translate their flags to
    /// the env twins first (the compatibility input), then [`freeze`] the result.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            ocap_disabled: Self::env_switch("NEWT_DISABLE_OCAP"),
            full_access: Self::env_switch("NEWT_FULL_ACCESS"),
            unsafe_host_exec: Self::env_switch("NEWT_UNSAFE_HOST_EXEC"),
        }
    }

    /// `--disable-ocap` / `--yolo` asserted for this process.
    #[must_use]
    pub const fn ocap_disabled(self) -> bool {
        self.ocap_disabled
    }

    /// `--full-access` asserted for this process.
    #[must_use]
    pub const fn full_access(self) -> bool {
        self.full_access
    }

    /// `--unsafe-host-exec` asserted for this process.
    #[must_use]
    pub const fn unsafe_host_exec(self) -> bool {
        self.unsafe_host_exec
    }

    /// Attenuate: the per-bit `meet` (logical AND) of two authorities. A switch
    /// is on in the result only if it was on in BOTH — so `meet` can drop
    /// authority but never add it. This is how a later context lowers authority
    /// without any path to widen it.
    #[must_use]
    pub const fn meet(self, other: Self) -> Self {
        Self {
            ocap_disabled: self.ocap_disabled && other.ocap_disabled,
            full_access: self.full_access && other.full_access,
            unsafe_host_exec: self.unsafe_host_exec && other.unsafe_host_exec,
        }
    }
}

// The frozen store is a process-global `OnceLock` in production; in test builds
// it is a **thread-local** so a freeze in one test cannot bleed into a
// concurrent test in the same binary (e.g. the `disable_ocap_tests` that drive
// `ocap_disabled()` via their env guards). The freeze/current logic is
// otherwise identical, and both are "first freeze wins".
#[cfg(not(test))]
static PROCESS_FROZEN: std::sync::OnceLock<LaunchAuthority> = std::sync::OnceLock::new();

#[cfg(test)]
thread_local! {
    static PROCESS_FROZEN: std::cell::Cell<Option<LaunchAuthority>> =
        const { std::cell::Cell::new(None) };
}

/// Freeze the launch authority for the process. Call this once, near startup,
/// AFTER the entrypoint has translated its authority flags to the env twins.
/// **First freeze wins**: a later call (or a later env mutation) can never widen
/// the running process's authority (a `OnceLock` ignores subsequent sets).
pub fn freeze(authority: LaunchAuthority) {
    #[cfg(not(test))]
    {
        PROCESS_FROZEN.set(authority).ok();
    }
    #[cfg(test)]
    {
        PROCESS_FROZEN.with(|c| {
            if c.get().is_none() {
                c.set(Some(authority));
            }
        });
    }
}

/// The launch authority for this process. Deep libraries call this to decide
/// authority instead of reading `std::env::var`.
///
/// If an entrypoint has [`freeze`]d (every production entrypoint does, right
/// after it translates its authority flags to the env twins), this returns that
/// frozen value and a later env mutation is ignored — the invariant. If nothing
/// has frozen yet it resolves live from env, which is only the pre-freeze
/// startup window in production and the normal state in a test that does not
/// freeze (so existing env-driven tests keep working; the frozen path is proven
/// by `frozen_authority_ignores_later_env_mutation`). It never lazily freezes —
/// that would let the *first* read in a long-lived test process pin the value
/// for every later test in the same binary.
#[must_use]
pub fn current() -> LaunchAuthority {
    #[cfg(not(test))]
    {
        PROCESS_FROZEN
            .get()
            .copied()
            .unwrap_or_else(LaunchAuthority::from_env)
    }
    #[cfg(test)]
    {
        PROCESS_FROZEN
            .with(std::cell::Cell::get)
            .unwrap_or_else(LaunchAuthority::from_env)
    }
}

/// Clear the thread-local frozen authority (test-only) so the freeze-path tests
/// run in isolation.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    PROCESS_FROZEN.with(|c| c.set(None));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes the tests here that touch the shared env twins + the frozen
    /// global, so the process-wide state can't race across the parallel runner.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// RAII env override restoring the previous value on drop (even on panic).
    struct EnvVar {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvVar {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, prev }
        }
    }
    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn from_env_reads_exactly_one_fail_closed() {
        let _g = serial();
        let _a = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let _b = EnvVar::set("NEWT_FULL_ACCESS", "true"); // not "1" ⇒ fail-closed
        let _c = EnvVar::unset("NEWT_UNSAFE_HOST_EXEC");
        let a = LaunchAuthority::from_env();
        assert!(a.ocap_disabled(), "exactly \"1\" enables");
        assert!(
            !a.full_access(),
            "a widening switch reads fail-closed unless == \"1\""
        );
        assert!(!a.unsafe_host_exec());
    }

    #[test]
    fn meet_can_only_attenuate_never_widen() {
        let full = LaunchAuthority {
            ocap_disabled: true,
            full_access: true,
            unsafe_host_exec: true,
        };
        // meeting with CONFINED clears everything…
        assert_eq!(
            full.meet(LaunchAuthority::CONFINED),
            LaunchAuthority::CONFINED
        );
        // …and a confined base can never be widened by meeting with `full`.
        assert_eq!(
            LaunchAuthority::CONFINED.meet(full),
            LaunchAuthority::CONFINED
        );
    }

    /// THE adversarial invariant: construct a confined authority, FREEZE it,
    /// then set every authority env var — `current()` must stay confined. This
    /// is the "authority cannot widen from a later environment mutation" bound.
    #[test]
    fn frozen_authority_ignores_later_env_mutation() {
        let _g = serial();
        reset_for_test();
        // Startup resolves confined (no switches).
        let _off1 = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _off2 = EnvVar::unset("NEWT_FULL_ACCESS");
        let _off3 = EnvVar::unset("NEWT_UNSAFE_HOST_EXEC");
        freeze(LaunchAuthority::from_env());
        assert_eq!(current(), LaunchAuthority::CONFINED, "froze confined");

        // A later actor sets every switch — the frozen value must not move.
        let _on1 = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let _on2 = EnvVar::set("NEWT_FULL_ACCESS", "1");
        let _on3 = EnvVar::set("NEWT_UNSAFE_HOST_EXEC", "1");
        let now = current();
        assert!(
            !now.ocap_disabled(),
            "later NEWT_DISABLE_OCAP must not widen a frozen confined authority"
        );
        assert!(!now.full_access(), "later NEWT_FULL_ACCESS must not widen");
        assert!(
            !now.unsafe_host_exec(),
            "later NEWT_UNSAFE_HOST_EXEC must not widen"
        );
        reset_for_test();
    }

    #[test]
    fn freeze_is_first_wins_a_second_freeze_cannot_widen() {
        let _g = serial();
        reset_for_test();
        freeze(LaunchAuthority::CONFINED);
        freeze(LaunchAuthority {
            ocap_disabled: true,
            full_access: true,
            unsafe_host_exec: true,
        });
        assert_eq!(
            current(),
            LaunchAuthority::CONFINED,
            "the first (confined) freeze wins"
        );
        reset_for_test();
    }
}
