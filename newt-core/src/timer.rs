//! Self-scheduled wake-up timers — the agent's "check back in 5 minutes"
//! substrate.
//!
//! A timer is a deferred prompt + a fire time, persisted as JSON at
//! `<newt config dir>/timers.json`. The agent (or an operator) schedules one;
//! a host-side caller (`newt timer fire --run`, or a cron/launchd job wrapping
//! it) drains due timers and feeds the prompt back into a headless run. This is
//! the minimum viable "wake myself up" loop — the agent is not resident
//! between turns, so the **host** owns the clock, not the model.
//!
//! ## Lifecycle: select/claim → execute → acknowledge
//!
//! A due timer is never consumed merely because it was selected. The host
//! flow is:
//!
//! 1. **claim** the next due timer ([`TimerStore::claim_next_due`]) — it is
//!    marked in-flight (`claimed_at`) so a concurrent beat cannot re-fire it;
//! 2. **execute** it (`newt solve`, driven by `newt timer fire --run`);
//! 3. **acknowledge** ([`TimerStore::acknowledge`]) — on success a one-shot is
//!    removed and a repeating timer advances; on failure the claim is released
//!    and the timer stays pending/retryable. Execution stops at the first
//!    failure so later due timers are never silently lost — they remain
//!    pending (unclaimed) for the next beat.
//!
//! The decision core ([`select_due`], [`select_claimable`], [`schedule_new`],
//! [`advance_repeat`], [`acknowledge_success`], [`acknowledge_failure`]) is
//! pure and fully unit-tested with an injected [`Clock`] — no real fs, no
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

/// How long a claim stays "fresh" (excluded from re-selection). Bounds crash
/// recovery: a timer claimed by a `fire --run` that died mid-execute becomes
/// re-selectable once its claim is older than this, instead of being stranded
/// forever. One hour is far longer than any sane solve round.
pub const CLAIM_FRESH_SECS: u64 = 3600;

/// Opaque timer id (`tm_<seq>_<created>`). `<seq>` is monotonic across
/// deletions (see [`next_seq`]) so ids never collide.
pub type TimerId = String;

/// One scheduled wake-up.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timer {
    /// Stable id (`tm_<seq>_<created>`); `<seq>` is [`next_seq`]-derived.
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
    /// `Some(t)` while a `fire --run` beat has claimed this timer for
    /// execution. `now - t < CLAIM_FRESH_SECS` ⇒ in-flight (not re-selectable);
    /// older ⇒ a stale claim from a dead run (re-selectable). `None` ⇒ idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<u64>,
}

/// Pure: which timers are due at `now` (`fire_at <= now`), in creation order.
/// Ignores claim state — see [`select_claimable`] for the claim-aware view.
pub fn select_due(timers: &[Timer], now: u64) -> Vec<&Timer> {
    let mut due: Vec<&Timer> = timers.iter().filter(|t| t.fire_at <= now).collect();
    due.sort_by_key(|t| (t.created_at, t.fire_at));
    due
}

/// Pure: due timers that are claimable — not freshly claimed. A stale claim
/// (older than [`CLAIM_FRESH_SECS`]) is treated as idle so a crashed
/// `fire --run` cannot strand a timer forever.
pub fn select_claimable(timers: &[Timer], now: u64) -> Vec<&Timer> {
    select_due(timers, now)
        .into_iter()
        .filter(|t| is_claimable(t, now))
        .collect()
}

/// Pure: is `t` claimable at `now` (idle or stale claim)?
#[must_use]
pub fn is_claimable(t: &Timer, now: u64) -> bool {
    match t.claimed_at {
        None => true,
        Some(c) => now.saturating_sub(c) >= CLAIM_FRESH_SECS,
    }
}

/// The next sequence number for a new timer id. Derived from the **maximum
/// existing seq** in `timers` (parsed from each `tm_<seq>_<created>` id) + 1,
/// NOT from `timers.len()`: deleting a timer and scheduling another must not
/// collide with a still-existing id. Falls back to 1 for an empty/unparseable
/// queue.
#[must_use]
pub fn next_seq(timers: &[Timer]) -> u64 {
    timers
        .iter()
        .filter_map(|t| t.id.strip_prefix("tm_"))
        .filter_map(|rest| rest.split('_').next())
        .filter_map(|s| s.parse::<u64>().ok())
        .max()
        .map_or(1, |m| m.saturating_add(1))
}

/// Pure: build a new timer firing `after_secs` from `now`. The id is
/// `tm_<seq>_<created>` where `seq` is [`next_seq`] — unique across deletions,
/// never derived solely from `timers.len()`.
pub fn schedule_new(
    timers: &[Timer],
    after_secs: u64,
    prompt: &str,
    now: u64,
    repeat_secs: Option<u64>,
) -> Timer {
    let seq = next_seq(timers);
    Timer {
        id: format!("tm_{seq}_{now}"),
        prompt: prompt.to_owned(),
        fire_at: now.saturating_add(after_secs),
        created_at: now,
        repeat_secs,
        claimed_at: None,
    }
}

/// Pure: after a repeating timer fires at/before `now`, re-arm it to the first
/// firing strictly after `now`. Returns `None` for one-shots and for a
/// malformed zero-step timer (treated as a one-shot — dismiss on fire — rather
/// than spinning). Uses bounded O(1) arithmetic, not an iterative loop.
pub fn advance_repeat(timer: &Timer, now: u64) -> Option<Timer> {
    let step = timer.repeat_secs?;
    // Defensive: a zero (or malformed) repeat interval would never advance
    // `fire_at`, so an iterative re-arm would spin forever once the job is due.
    // `TimerStore::schedule` and the CLI reject zero repeats at the validation
    // boundary; this guards malformed persisted state. Treat a zero-step timer
    // as a one-shot — dismiss on fire — rather than spinning.
    if step == 0 {
        return None;
    }
    let mut next = timer.clone();
    if next.fire_at > now {
        // Not actually due; leave the firing time alone.
        return Some(next);
    }
    // Deficit = how far past fire_at we are. Advance by ceil(deficit/step)
    // whole steps so the new fire_at is the first firing strictly after `now`.
    // Bounded arithmetic: `deficit/step + 1` steps in O(1), checked/saturating
    // so a huge deficit or near-overflow fire_at cannot wrap.
    let deficit = now - next.fire_at;
    let steps = deficit / step + 1;
    let advance = match steps.checked_mul(step) {
        Some(a) => a,
        None => {
            next.fire_at = u64::MAX;
            next.claimed_at = None;
            return Some(next);
        }
    };
    next.fire_at = next.fire_at.saturating_add(advance);
    next.claimed_at = None;
    Some(next)
}

/// Pure: the post-success state of a timer after its wake/solve succeeded.
/// One-shot → `None` (removed). Repeating → the advanced timer (claim cleared).
#[must_use]
pub fn acknowledge_success(timer: &Timer, now: u64) -> Option<Timer> {
    match timer.repeat_secs {
        Some(_) => advance_repeat(timer, now),
        None => None,
    }
}

/// Pure: the post-failure state — claim released, `fire_at` unchanged, so the
/// timer stays pending and retryable on the next beat.
#[must_use]
pub fn acknowledge_failure(timer: &Timer) -> Timer {
    let mut t = timer.clone();
    t.claimed_at = None;
    t
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
        // Config/persistence boundary: a zero repeat interval would make
        // `advance_repeat` spin forever once the job is due. Reject it here so
        // no caller — not just the CLI — can persist one.
        if let Some(0) = repeat_secs {
            anyhow::bail!("repeat interval must be greater than zero");
        }
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

    /// Non-mutating: the claimable due timers (earliest first), for bare
    /// `fire` emit / inspection. Does NOT consume or claim — a due timer is
    /// not removed merely for being emitted.
    ///
    /// # Errors
    /// Propagates load IO errors.
    pub fn due(&self, clock: &dyn Clock) -> anyhow::Result<Vec<Timer>> {
        let now = clock.now_secs();
        let timers = self.load()?;
        let mut due: Vec<Timer> = select_claimable(&timers, now)
            .into_iter()
            .cloned()
            .collect();
        due.sort_by_key(|t| (t.created_at, t.fire_at));
        Ok(due)
    }

    /// **select/claim**: claim the next claimable due timer (mark
    /// `claimed_at = now`, persist), returning it for execution. `None` when
    /// nothing is due/claimable. The caller must execute the prompt then call
    /// [`Self::acknowledge`] — a timer is never consumed merely for being
    /// claimed.
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn claim_next_due(&self, clock: &dyn Clock) -> anyhow::Result<Option<Timer>> {
        let now = clock.now_secs();
        let mut timers = self.load()?;
        let pick = select_claimable(&timers, now)
            .into_iter()
            .min_by_key(|t| (t.created_at, t.fire_at));
        let Some(pick) = pick else {
            return Ok(None);
        };
        let target = pick.id.clone();
        for t in &mut timers {
            if t.id == target {
                t.claimed_at = Some(now);
                break;
            }
        }
        self.save(&timers)?;
        let claimed = timers
            .into_iter()
            .find(|t| t.id == target)
            .expect("just set");
        Ok(Some(claimed))
    }

    /// **acknowledge**: the outcome of executing `id`.
    /// - success: a one-shot is removed; a repeating timer advances (claim
    ///   cleared).
    /// - failure: the claim is released and the timer stays pending/retryable
    ///   (`fire_at` unchanged).
    ///
    /// Returns whether `id` matched a timer.
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn acknowledge(&self, id: &str, success: bool, clock: &dyn Clock) -> anyhow::Result<bool> {
        let now = clock.now_secs();
        let timers = self.load()?;
        let mut kept: Vec<Timer> = Vec::with_capacity(timers.len());
        let mut found = false;
        for t in timers {
            if !found && t.id == id {
                found = true;
                let replacement = if success {
                    acknowledge_success(&t, now)
                } else {
                    Some(acknowledge_failure(&t))
                };
                if let Some(r) = replacement {
                    kept.push(r);
                }
                // None ⇒ one-shot succeeded ⇒ removed.
            } else {
                kept.push(t);
            }
        }
        self.save(&kept)?;
        Ok(found)
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
            claimed_at: None,
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
        assert!(timer.claimed_at.is_none());
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
        assert!(next.claimed_at.is_none());
        // one-shot returns None
        let one = t("b", "p", 100, 0, None);
        assert!(advance_repeat(&one, 250).is_none());
    }

    #[test]
    fn schedule_new_saturates_overflow() {
        let timer = schedule_new(&[], u64::MAX, "p", 1, None);
        assert_eq!(timer.fire_at, u64::MAX);
    }

    #[test]
    fn advance_repeat_zero_step_dismisses_not_spins() {
        // Regression (#1747): a zero repeat interval must never spin
        // `advance_repeat` forever. A malformed persisted timer with
        // `repeat_secs == Some(0)` is treated as a one-shot (returns `None`)
        // — both when it is freshly due and when it is already past `now`.
        let zero_step = t("a", "p", 100, 0, Some(0));
        assert!(advance_repeat(&zero_step, 100).is_none());
        assert!(advance_repeat(&zero_step, 1).is_none());
    }

    /// Regression (#1747 req 8/9): timer ids must be actually unique — never
    /// derived solely from `timers.len()`. After deleting a timer and
    /// scheduling another, the new id must not collide with a still-existing
    /// one. The old `timers.len()+1` scheme collided here: deleting `tm_2`
    /// left `len == 2`, so the next id was `tm_3` — already taken.
    #[test]
    fn next_seq_survives_deletion_no_collision() {
        let mut timers = vec![
            t("tm_1_0", "a", 0, 0, None),
            t("tm_2_0", "b", 0, 0, None),
            t("tm_3_0", "c", 0, 0, None),
        ];
        // Delete tm_2 (as `dismiss` would).
        timers.retain(|x| x.id != "tm_2_0");
        // Next seq is max(1, 3) + 1 = 4 — NOT len()+1 = 3 (which collides).
        assert_eq!(next_seq(&timers), 4);
        let new = schedule_new(&timers, 10, "d", 5, None);
        assert_eq!(new.id, "tm_4_5");
        assert!(timers.iter().all(|x| x.id != new.id), "no id collision");
    }

    /// Regression (#1747 req 10): `advance_repeat` jumps directly to the first
    /// fire time after `now` with bounded O(1) arithmetic, not an iterative
    /// catch-up loop. A large deficit resolves in one step.
    #[test]
    fn advance_repeat_jumps_directly_past_now_bounded() {
        let timer = t("a", "p", 100, 0, Some(60));
        // fire_at=100, now=10_000: deficit=9900, steps=9900/60+1=166,
        // advance=166*60=9960, new fire_at=100+9960=10_060 (first > 10_000).
        let next = advance_repeat(&timer, 10_000).unwrap();
        assert_eq!(next.fire_at, 10_060);
        // Exact step boundary: now=10_060 ⇒ first strictly after is 10_120.
        let next2 = advance_repeat(&next, 10_060).unwrap();
        assert_eq!(next2.fire_at, 10_120);
    }

    /// Regression (#1747 req 11): bounded arithmetic saturates on overflow
    /// instead of wrapping or spinning.
    #[test]
    fn advance_repeat_overflow_saturates() {
        // step huge, fire_at near MAX: deficit/step + 1 steps overflow checked
        // arithmetic → saturate to u64::MAX.
        let timer = t("a", "p", u64::MAX - 10, 0, Some(u64::MAX));
        let next = advance_repeat(&timer, u64::MAX).unwrap();
        assert_eq!(next.fire_at, u64::MAX);
    }

    /// (#1747 req 13) one-shot acknowledgement: success removes (None),
    /// failure keeps pending with the claim released.
    #[test]
    fn acknowledge_one_shot_semantics() {
        let one = t("tm_1_0", "p", 100, 0, None);
        assert!(
            acknowledge_success(&one, 200).is_none(),
            "one-shot success removes"
        );
        let failed = acknowledge_failure(&one);
        assert_eq!(failed.fire_at, 100, "failure keeps fire_at");
        assert!(failed.claimed_at.is_none(), "failure releases claim");
        assert_eq!(failed.id, "tm_1_0");
    }

    /// (#1747 req 13) repeating acknowledgement: success advances past now and
    /// clears the claim; failure keeps the firing time and releases the claim.
    #[test]
    fn acknowledge_repeating_semantics() {
        let rep = Timer {
            id: "tm_1_0".into(),
            prompt: "p".into(),
            fire_at: 100,
            created_at: 0,
            repeat_secs: Some(60),
            claimed_at: Some(100), // claimed by the beat
        };
        let advanced = acknowledge_success(&rep, 250).expect("repeating stays");
        assert_eq!(advanced.fire_at, 280, "advances to first > now");
        assert!(advanced.claimed_at.is_none(), "success clears claim");
        assert_eq!(advanced.repeat_secs, Some(60));

        let failed = acknowledge_failure(&rep);
        assert_eq!(failed.fire_at, 100, "failure keeps fire_at");
        assert!(failed.claimed_at.is_none(), "failure releases claim");
    }

    /// (#1747 req 4) claim awareness: a freshly-claimed due timer is not
    /// re-selectable; a stale claim (older than CLAIM_FRESH_SECS) is.
    #[test]
    fn select_claimable_respects_fresh_and_stale_claims() {
        let idle = t("tm_1_0", "a", 100, 0, None);
        let fresh = Timer {
            claimed_at: Some(100),
            ..t("tm_2_0", "b", 100, 1, None)
        };
        let stale = Timer {
            claimed_at: Some(100),
            ..t("tm_3_0", "c", 100, 2, None)
        };
        let timers = vec![idle.clone(), fresh, stale];
        // now=100: fresh claim excluded, stale (100 - 100 = 0 < 3600) also fresh-ish.
        let now_fresh = 100u64;
        let ids: Vec<&str> = select_claimable(&timers, now_fresh)
            .iter()
            .map(|x| x.id.as_str())
            .collect();
        assert_eq!(ids, ["tm_1_0"], "fresh claims excluded at now=100");

        // now = 100 + CLAIM_FRESH_SECS: tm_3 (claimed at 100) is stale → selectable;
        // tm_2 (claimed at 100) also stale now → selectable. All three return.
        let now_stale = 100 + CLAIM_FRESH_SECS;
        let ids2: Vec<&str> = select_claimable(&timers, now_stale)
            .iter()
            .map(|x| x.id.as_str())
            .collect();
        assert_eq!(
            ids2,
            ["tm_1_0", "tm_2_0", "tm_3_0"],
            "stale claims re-selectable"
        );
        let _ = idle;
    }

    /// (#1747 req 7/12) multiple due timers, one execution fails: the failed
    /// timer stays pending/retryable and the unexecuted later timers are NOT
    /// silently lost — they remain pending (idle) for the next beat. Simulates
    /// the host claim → execute(fail) → acknowledge(failure) → stop flow
    /// against the pure core; no fs, no wall-clock.
    #[test]
    fn failed_execution_keeps_later_timers_pending() {
        let now = 100;
        let mut timers = vec![
            t("tm_1_0", "A", 10, 0, None),
            t("tm_2_0", "B", 20, 1, None),
            t("tm_3_0", "C", 30, 2, None),
        ];
        // All three are due & claimable at now=100.
        assert_eq!(
            select_claimable(&timers, now)
                .iter()
                .map(|x| x.id.as_str())
                .collect::<Vec<_>>(),
            ["tm_1_0", "tm_2_0", "tm_3_0"]
        );
        // Host claims A (earliest).
        timers[0].claimed_at = Some(now);
        // A's solve fails → acknowledge failure → claim released, fire_at kept.
        timers[0] = acknowledge_failure(&timers[0]);
        assert_eq!(timers[0].fire_at, 10);
        assert!(timers[0].claimed_at.is_none());
        // Beat stops. B and C were never claimed/acked. All three must still be
        // pending & claimable — none lost, none advanced.
        let pending: Vec<&str> = select_claimable(&timers, now)
            .iter()
            .map(|x| x.id.as_str())
            .collect();
        assert_eq!(pending, ["tm_1_0", "tm_2_0", "tm_3_0"]);
        assert!(timers.iter().all(|x| x.claimed_at.is_none()));
    }

    /// (#1747 req 12) the success path drains every due timer: none is lost.
    /// Simulates claim → execute(ok) → acknowledge(success) for three
    /// one-shots; each is removed. A repeating timer would advance instead.
    #[test]
    fn successful_drain_acks_every_due_timer() {
        let now = 100;
        let mut timers = vec![
            t("tm_1_0", "A", 10, 0, None),
            t("tm_2_0", "B", 20, 1, None),
            t("tm_3_0", "C", 30, 2, None),
        ];
        // Claim + succeed the earliest due timer, one at a time, until none due.
        let mut drained = 0;
        loop {
            // Earliest due timer (creation order), idle.
            let pick = select_claimable(&timers, now)
                .into_iter()
                .min_by_key(|x| (x.created_at, x.fire_at));
            let Some(pick) = pick else { break };
            let id = pick.id.clone();
            let pos = timers.iter().position(|x| x.id == id).unwrap();
            timers[pos].claimed_at = Some(now); // claim
            match acknowledge_success(&timers[pos], now) {
                None => timers.retain(|x| x.id != id), // one-shot removed
                Some(r) => {
                    let p = timers.iter().position(|x| x.id == id).unwrap();
                    timers[p] = r; // repeating advanced
                }
            }
            drained += 1;
        }
        assert_eq!(drained, 3, "all three one-shots drained on success");
        assert!(timers.is_empty(), "queue empty after successful drain");
    }
}
