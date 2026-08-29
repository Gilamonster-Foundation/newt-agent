//! **The one lock over the process environment** (#1850).
//!
//! `std::env::set_var` is `unsafe` because the environment is process-global
//! mutable state that `getenv` reads without synchronization. Newt mutates it
//! deliberately: the REPL's `/backends`, `/model`, `/prompt`, `/nudge` and
//! persona routing publish the next turn's selection through `NEWT_PROVIDER`,
//! `NEWT_DGX_MODEL`, `NEWT_BACKEND`, `NEWT_OPENAI_API` and friends, and the
//! resolution path reads them back.
//!
//! ## What went wrong
//!
//! Before #1850 there were **two** locks over that one environment, plus a
//! population of writers holding neither:
//!
//! - `newt_core::test_guard::GlobalSettingsGuard` — a `Mutex`, snapshotting
//!   `NEWT_PROVIDER` / `NEWT_DGX_MODEL` / `NEWT_OPENAI_API` / `NEWT_TEAM`.
//! - `newt_tui`'s private `test_env_guard` — an independent `RwLock`.
//! - Every production writer, under `// SAFETY: single-threaded REPL`.
//!
//! Neither lock excluded the other, so a test holding one raced a test holding
//! the other, and both raced production. The observed cost on `main` was a
//! ~30%-of-runs flake in `cargo test -p newt-tui --lib --all-features` that
//! took down whole modules at once: `tab_switch::state_machine_tests` (30
//! tests, holding `GlobalSettingsGuard`) failed wholesale when a sibling
//! holding the other lock set `NEWT_DGX_MODEL` mid-snapshot, and
//! `helper_fn_tests::resolver_default_backend_beats_the_openai_heuristic`
//! observed a literal `"bound-model"` that exists nowhere but another module's
//! env fixture.
//!
//! The `// SAFETY: single-threaded REPL` justification is the part worth
//! naming. It is TRUE of the REPL and FALSE under `cargo test`, which runs
//! tests as parallel threads of one process — so it was a safety claim that
//! held exactly where nobody was checking and failed exactly where the
//! evidence was. A justification that is false in the harness that runs it is
//! not a justification.
//!
//! ## What this module is
//!
//! One lock, taken by everyone who mutates the environment — production and
//! tests alike — so serialization is an *invariant* rather than a convention
//! each call site must remember. Production sites call [`set_var`],
//! [`remove_var`] and [`set_or_remove`], which own the `unsafe` and the lock
//! together; tests take [`lock`] (through `GlobalSettingsGuard` or `newt-tui`'s
//! `test_env_guard`, both of which now delegate here) to hold the environment
//! still across a whole scenario.
//!
//! ## Why REENTRANT
//!
//! Tests legitimately hold the environment still and then drive production
//! code that writes it — `apply_backend_choice_refuses_embedded_before_any_mutation`
//! and `browsing_backends_marks_no_preference_action` both do exactly that. A
//! plain mutex would deadlock on the second acquisition. `ReentrantMutex`
//! re-admits the thread that already holds it while still excluding every
//! other one, which is precisely the property those tests need and the
//! property the race needs removed.
//!
//! `parking_lot` is already in the build graph (a transitive dependency), so
//! this adds no new supply-chain surface — and a hand-rolled reentrant lock
//! inside a fix for a concurrency bug is the last thing this wants to be.
//!
//! ## What the lock does NOT buy
//!
//! It serializes newt's own writers against newt's own guarded readers. It
//! cannot stop a `getenv` inside libc, a third-party crate, or an unguarded
//! read elsewhere in this process from observing a torn update — that is a
//! property of the C environment, not of this lock. The genuinely sound fix is
//! to stop round-tripping session state through the environment at all and
//! thread it explicitly; that is tracked as the follow-up to this slice
//! (#1851). Until then, this is the strongest invariant available that is also
//! true.

use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use std::cell::Cell;

/// The one lock. Everything in this module goes through it.
static ENV_LOCK: ReentrantMutex<()> = ReentrantMutex::new(());

thread_local! {
    /// How many [`EnvGuard`]s this thread currently holds.
    ///
    /// `parking_lot` does not publish its owner, and "does THIS thread hold
    /// the environment?" is a question the regression tests need answered
    /// deterministically. A cross-thread `try_lock` probe cannot answer it: a
    /// failed probe means *somebody* holds the lock, which a sibling test can
    /// satisfy just as well as we can — so a test built on one can pass for
    /// the wrong reason. This counter is thread-local, so it cannot.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// A hold on the process environment.
///
/// A real type rather than a `parking_lot` alias, so callers — `test_guard`
/// here, `newt-tui`'s `test_env_guard` — name the guard without taking their
/// own `parking_lot` dependency, and so the depth bookkeeping cannot be
/// bypassed by constructing the inner guard directly.
pub struct EnvGuard {
    _inner: ReentrantMutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

fn track(inner: ReentrantMutexGuard<'static, ()>) -> EnvGuard {
    DEPTH.with(|d| d.set(d.get() + 1));
    EnvGuard { _inner: inner }
}

/// Hold the process environment still for the caller's thread.
///
/// Re-entrant: a thread that already holds it may take it again (the guards
/// nest, and the environment is released when the outermost drops). Every
/// other thread blocks.
pub fn lock() -> EnvGuard {
    track(ENV_LOCK.lock())
}

/// Take the lock if it is free, without blocking.
pub fn try_lock() -> Option<EnvGuard> {
    ENV_LOCK.try_lock().map(track)
}

/// Whether the CALLING thread currently holds the process environment.
///
/// The deterministic form of "are the two guard families the same lock?" —
/// take one family's guard and ask this. See the #1850 regression tests.
#[must_use]
pub fn held_by_current_thread() -> bool {
    DEPTH.with(Cell::get) > 0
}

/// Set `key` to `value` under the lock.
pub fn set_var(key: &str, value: &str) {
    let _g = lock();
    // SAFETY: the process-env lock is held, so no other newt writer and no
    // guarded reader is inside the environment for the duration of this call.
    // (Module docs: this excludes newt's own racers, not libc's `getenv`.)
    unsafe { std::env::set_var(key, value) };
}

/// Remove `key` under the lock.
pub fn remove_var(key: &str) {
    let _g = lock();
    // SAFETY: as `set_var` — the lock is held for the write.
    unsafe { std::env::remove_var(key) };
}

/// Set `key` to `value`, or remove it when `value` is `None` — one atomic
/// step under the lock.
///
/// This is the shape nearly every call site actually wants: a session axis is
/// either pinned to something or reverted to unset, and writing it as two
/// separate `set`/`remove` arms at four sites is how the arms drift.
pub fn set_or_remove(key: &str, value: Option<&str>) {
    match value {
        Some(v) => set_var(key, v),
        None => remove_var(key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock re-admits its own holder — the property the guarded tests
    /// that drive production writers depend on. A plain mutex deadlocks here,
    /// which is why this is asserted rather than assumed.
    #[test]
    fn the_lock_is_reentrant_on_one_thread() {
        assert!(
            !held_by_current_thread(),
            "a fresh test thread holds nothing"
        );
        let outer = lock();
        assert!(held_by_current_thread());
        {
            let _inner = lock();
            assert!(
                try_lock().is_some(),
                "the holding thread may always take it again"
            );
        }
        assert!(
            held_by_current_thread(),
            "an inner guard's drop must not release the outer hold"
        );
        drop(outer);
        assert!(!held_by_current_thread(), "the outermost drop releases it");
    }

    /// …and excludes every other thread, which is the half that fixes #1850.
    ///
    /// Deterministic: THIS thread holds the lock, so no other one can take it,
    /// whatever the scheduler does with the rest of the suite.
    ///
    /// There is deliberately **no** `drop(held)` + "another thread can get in
    /// now" probe. It reads as the obvious second half and it is unassertable
    /// here (#1872): between the drop and the probe, any sibling holding the
    /// process-wide lock — `test_guard::GlobalSettingsGuard`, or any test
    /// driving a production writer — makes `try_lock` return `None`, and the
    /// assertion fails for a reason with nothing to do with this module.
    /// Measured on `main`: 0/40 runs at the default thread count, 6/20 at
    /// `--test-threads=128`, **11/15 at 256** — sibling contention, dialled.
    ///
    /// Release is covered instead by `the_lock_is_reentrant_on_one_thread`,
    /// thread-locally and without a race. That leaves exactly one sliver: the
    /// inner `parking_lot` guard leaking while `DEPTH` still decrements. It is
    /// not worth a probe — reached only by deliberately suppressing the drop
    /// glue, and it wedges the whole test binary (verified: 9s run, still hung
    /// at 90s) rather than passing silently.
    #[test]
    fn the_lock_excludes_other_threads() {
        let held = lock();
        let taken = std::thread::spawn(|| try_lock().is_some())
            .join()
            .expect("probe thread");
        assert!(!taken, "another thread must not enter the environment");
        drop(held);
    }

    /// `set_or_remove` is the two arms as one call, so they cannot drift.
    #[test]
    fn set_or_remove_covers_both_arms() {
        const KEY: &str = "NEWT_PROCESS_ENV_SELFTEST";
        let _g = lock();
        set_or_remove(KEY, Some("on"));
        assert_eq!(std::env::var(KEY).ok().as_deref(), Some("on"));
        set_or_remove(KEY, None);
        assert!(std::env::var(KEY).is_err());
    }
}
