//! Herdr lifecycle reporting.
//!
//! When newt runs inside a [Herdr](https://herdr.dev) pane, Herdr injects
//! `HERDR_ENV=1` and `HERDR_PANE_ID` into the environment. This module detects
//! that and reports the agent's lifecycle state (`idle` / `working` /
//! `blocked`) through `herdr pane report-agent`, which Herdr treats as
//! authoritative — no screen-scraping heuristics needed. Outside Herdr every
//! call here is a no-op, resolved once through a `OnceLock` (same shape as
//! `terminal_hyperlink::supports_osc8`).
//!
//! Reports are fire-and-forget: the `herdr` CLI is spawned detached with all
//! stdio nulled, and a reaper thread waits on it so no zombies accumulate. A
//! failure to spawn (e.g. `herdr` not on PATH) is silently ignored — lifecycle
//! telemetry must never affect the session.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) const IDLE: &str = "idle";
pub(crate) const WORKING: &str = "working";
pub(crate) const BLOCKED: &str = "blocked";

/// The pane ID to report against, or `None` when not running under Herdr.
fn herdr_pane() -> Option<&'static str> {
    static PANE: OnceLock<Option<String>> = OnceLock::new();
    PANE.get_or_init(|| {
        if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
            return None;
        }
        std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())
    })
    .as_deref()
}

/// The last state actually reported; used both to dedupe and to restore the
/// pre-prompt state when a blocking prompt window closes.
static LAST_STATE: Mutex<&'static str> = Mutex::new("");

/// Wire up Herdr reporting for this session. Call once at chat startup.
///
/// Registers the tty-arbiter prompt observer so every human-blocking prompt
/// (permission gate, question, live-spill modal) reports `blocked` on open and
/// restores the prior state on close — no per-call-site instrumentation.
pub(crate) fn init() {
    if herdr_pane().is_none() {
        return;
    }
    static PRE_PROMPT: Mutex<&'static str> = Mutex::new(WORKING);
    newt_core::tty::set_prompt_observer(|open| {
        if open {
            let current = *LAST_STATE.lock().unwrap_or_else(|p| p.into_inner());
            *PRE_PROMPT.lock().unwrap_or_else(|p| p.into_inner()) = current;
            report(BLOCKED);
        } else {
            let prior = *PRE_PROMPT.lock().unwrap_or_else(|p| p.into_inner());
            report(prior);
        }
    });
    report(IDLE);
}

/// Dedupe gate: records `state` as the last reported state and says whether a
/// report should actually be sent (i.e. the state changed).
fn should_report(state: &'static str) -> bool {
    let mut last = LAST_STATE.lock().unwrap_or_else(|p| p.into_inner());
    if *last == state {
        return false;
    }
    *last = state;
    true
}

/// The exact `herdr` CLI arguments for one lifecycle report.
fn report_args(pane: &str, state: &str, seq: u64) -> [String; 11] {
    [
        "pane".into(),
        "report-agent".into(),
        pane.into(),
        "--source".into(),
        "custom:newt".into(),
        "--agent".into(),
        "newt".into(),
        "--state".into(),
        state.into(),
        "--seq".into(),
        seq.to_string(),
    ]
}

/// Report a lifecycle state to Herdr. No-op outside Herdr or when the state
/// has not changed since the last report.
pub(crate) fn report(state: &'static str) {
    let Some(pane) = herdr_pane() else {
        return;
    };
    if !should_report(state) {
        return;
    }
    // Monotonic sequence number so Herdr can order reports even if the
    // spawned CLI processes complete out of order.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let spawned = Command::new("herdr")
        .args(report_args(pane, state, seq))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test owns the whole LAST_STATE sequence: the global is
    /// order-dependent, so splitting these assertions across parallel test
    /// functions would race.
    #[test]
    fn should_report_dedupes_repeats_and_allows_transitions() {
        // Fresh process state: "" → idle fires.
        assert!(should_report(IDLE));
        // Repeat is suppressed.
        assert!(!should_report(IDLE));
        // Every real transition fires, including returning to an earlier state.
        assert!(should_report(WORKING));
        assert!(should_report(BLOCKED));
        assert!(should_report(WORKING));
        assert!(!should_report(WORKING));
        assert!(should_report(IDLE));
    }

    #[test]
    fn report_args_match_the_herdr_cli_contract() {
        let args = report_args("w1:p2", BLOCKED, 7);
        assert_eq!(
            args,
            [
                "pane",
                "report-agent",
                "w1:p2",
                "--source",
                "custom:newt",
                "--agent",
                "newt",
                "--state",
                "blocked",
                "--seq",
                "7",
            ]
        );
    }
}
