//! Self-scheduled wake-up timers — the agent's "check back in 5 minutes"
//! substrate.
//!
//! A timer is a deferred prompt + a fire time, persisted as JSON at
//! `<newt config dir>/timers.json`. The agent (or an operator) schedules one;
//! a host-side caller (`newt timer fire`, or a cron/launchd job wrapping it)
//! drains due timers and feeds the prompt back into a headless run. This is
//! the minimum viable "wake myself up" loop — the agent is not resident
//! between turns, so the **host** owns the clock, not the model.
//!
//! The decision core ([`select_due`], [`schedule_new`], [`advance_repeat`])
//! is pure and fully unit-tested with an injected [`Clock`] — no real fs, no
//! wall-clock. The fs-touching [`TimerStore`] is a thin load/save wrapper
//! (same doctrine as `ocap_cmd`: the entry point is thin, the core is pure).
//!
//! Reuses [`crate::Config::user_config_dir`] for the root and plain JSON for
//! the envelope — integrity is not on the table for a prompt queue, so the
//! tamper-evident machinery of `store.rs` is deliberately not pulled in.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A unix-seconds clock so the decision core can be tested without wall-clock.
pub trait Clock {
    /// Seconds since the unix epoch.
    fn now_secs(&self) -> u64;
}

/// The real wall-clock. [`Clock`] for production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Opaque timer id (`tm_<seq>_<created>`). Deterministic, unique within a
/// store load without needing a random source.
pub type TimerId = String;

/// One scheduled wake-up.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timer {
    /// Stable id (`tm_<seq>_<created>`).
    pub id: TimerId,
    /// The prompt to re-enter the agent with when this timer fires.
    pub prompt: String,
    /// Unix seconds at which to fire.
    pub fire_at: u64,
    /// Unix seconds the timer was created.
    pub created_at: u64,
    /// If set, re-arm after this many seconds instead of dismissing on fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_secs: Option<u64>,
}

/// Pure: which timers are due at `now` (`fire_at <= now`), in creation order.
pub fn select_due(timers: &[Timer], now: u64) -> Vec<&Timer> {
    let mut due: Vec<&Timer> = timers.iter().filter(|t| t.fire_at <= now).collect();
    due.sort_by_key(|t| (t.created_at, t.fire_at));
    due
}

/// Pure: build a new timer firing `after_secs` from `now`.
///
/// The id derives from the existing count so it is unique within a store
/// load without a random source — `schedule_new` is deterministic for tests.
pub fn schedule_new(
    timers: &[Timer],
    after_secs: u64,
    prompt: &str,
    now: u64,
    repeat_secs: Option<u64>,
) -> Timer {
    let seq = timers.len() as u64 + 1;
    Timer {
        id: format!("tm_{seq}_{now}"),
        prompt: prompt.to_owned(),
        fire_at: now.saturating_add(after_secs),
        created_at: now,
        repeat_secs,
    }
}

/// Pure: after a repeating timer fires at/before `now`, re-arm it to the next
/// future firing. Returns `None` for one-shots.
pub fn advance_repeat(timer: &Timer, now: u64) -> Option<Timer> {
    let step = timer.repeat_secs?;
    let mut next = timer.clone();
    // Roll forward until the next firing is strictly in the future.
    while next.fire_at <= now {
        next.fire_at = next.fire_at.saturating_add(step);
    }
    Some(next)
}

/// JSON file backing the timer queue. Thin load/save; decision logic is pure.
pub struct TimerStore {
    path: PathBuf,
}

impl TimerStore {
    /// Open the timer queue at `<dir>/timers.json`. Reuses
    /// [`crate::Config::user_config_dir`] when `dir` is `None`.
    ///
    /// # Errors
    /// Fails only when the newt config directory cannot be resolved.
    pub fn open(dir: Option<&Path>) -> anyhow::Result<Self> {
        let path = match dir {
            Some(d) => d.join("timers.json"),
            None => crate::Config::user_config_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot locate the newt config directory"))?
                .join("timers.json"),
        };
        Ok(Self { path })
    }

    /// Path accessor (for diagnostics).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the queue. A missing file → empty (a fresh install has no
    /// timers); a corrupt file is logged and treated as empty rather than
    /// fatal — a bad `timers.json` must never block the agent.
    ///
    /// # Errors
    /// Propagates non-`NotFound` IO errors (permissions, …).
    pub fn load(&self) -> anyhow::Result<Vec<Timer>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Timer>>(&bytes) {
                Ok(timers) => Ok(timers),
                Err(e) => {
                    tracing::warn!("timers.json unreadable, treating as empty: {e}");
                    Ok(Vec::new())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, timers: &[Timer]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(timers)?;
        std::fs::write(&self.path, bytes)?;
        Ok(())
    }

    /// Schedule a new timer firing `after_secs` from `clock.now_secs()`.
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn schedule(
        &self,
        after_secs: u64,
        prompt: &str,
        clock: &dyn Clock,
        repeat_secs: Option<u64>,
    ) -> anyhow::Result<Timer> {
        let now = clock.now_secs();
        let mut timers = self.load()?;
        let timer = schedule_new(&timers, after_secs, prompt, now, repeat_secs);
        timers.push(timer.clone());
        self.save(&timers)?;
        Ok(timer)
    }

    /// List all timers, soonest-firing first.
    ///
    /// # Errors
    /// Propagates load IO errors.
    pub fn list(&self) -> anyhow::Result<Vec<Timer>> {
        let mut timers = self.load()?;
        timers.sort_by_key(|t| t.fire_at);
        Ok(timers)
    }

    /// Drain due timers: return them (so the caller can feed each prompt to a
    /// headless run), dismiss one-shots, and re-arm repeating timers. The
    /// returned vec is in the order the prompts should fire (creation order).
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn fire_due(&self, clock: &dyn Clock) -> anyhow::Result<Vec<Timer>> {
        let now = clock.now_secs();
        let timers = self.load()?;
        let due = select_due(&timers, now);
        if due.is_empty() {
            return Ok(Vec::new());
        }
        let due_ids: std::collections::HashSet<&str> = due.iter().map(|t| t.id.as_str()).collect();
        let mut kept: Vec<Timer> = Vec::new();
        let mut fired: Vec<Timer> = Vec::new();
        for t in &timers {
            if due_ids.contains(t.id.as_str()) {
                fired.push(t.clone());
                if let Some(rearmed) = advance_repeat(t, now) {
                    kept.push(rearmed);
                }
            } else {
                kept.push(t.clone());
            }
        }
        self.save(&kept)?;
        fired.sort_by_key(|t| (t.created_at, t.fire_at));
        Ok(fired)
    }

    /// Remove a timer by id (or an unambiguous id prefix) — cancel a watch.
    /// Returns `true` if exactly one timer matched and was removed.
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn dismiss(&self, id_or_prefix: &str) -> anyhow::Result<bool> {
        let timers = self.load()?;
        let matches: Vec<&Timer> = timers
            .iter()
            .filter(|t| t.id == id_or_prefix || t.id.starts_with(id_or_prefix))
            .collect();
        if matches.len() != 1 {
            return Ok(false);
        }
        let target = matches[0].id.clone();
        let kept: Vec<Timer> = timers.into_iter().filter(|t| t.id != target).collect();
        self.save(&kept)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, prompt: &str, fire_at: u64, created_at: u64, repeat: Option<u64>) -> Timer {
        Timer {
            id: id.into(),
            prompt: prompt.into(),
            fire_at,
            created_at,
            repeat_secs: repeat,
        }
    }

    #[test]
    fn select_due_filters_and_orders_by_creation() {
        let timers = vec![
            t("a", "p1", 100, 50, None),
            t("b", "p2", 60, 10, None),
            t("c", "p3", 200, 5, None),
        ];
        let due = select_due(&timers, 100);
        // a (fire_at 100, created 50) and b (fire_at 60, created 10): by created.
        assert_eq!(
            due.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert!(select_due(&timers, 0).is_empty());
    }

    #[test]
    fn schedule_new_sets_fire_at_and_unique_seq() {
        let existing = vec![t("tm_1_0", "x", 0, 0, None)];
        let now = 1000;
        let timer = schedule_new(&existing, 300, "check ci", now, None);
        assert_eq!(timer.fire_at, 1300);
        assert_eq!(timer.created_at, now);
        assert_eq!(timer.id, "tm_2_1000");
        assert!(timer.prompt.contains("check ci"));
        // repeat carries through
        let rep = schedule_new(&existing, 60, "p", now, Some(300));
        assert_eq!(rep.repeat_secs, Some(300));
    }

    #[test]
    fn advance_repeat_rolls_forward_past_now() {
        let timer = t("a", "p", 100, 0, Some(60));
        // fired at now=250 -> firings 160, 220, 280 -> first > 250 is 280.
        let next = advance_repeat(&timer, 250).unwrap();
        assert_eq!(next.fire_at, 280);
        assert_eq!(next.repeat_secs, Some(60));
        // one-shot returns None
        let one = t("b", "p", 100, 0, None);
        assert!(advance_repeat(&one, 250).is_none());
    }

    #[test]
    fn schedule_new_saturates_overflow() {
        let timer = schedule_new(&[], u64::MAX, "p", 1, None);
        assert_eq!(timer.fire_at, u64::MAX);
    }
}
