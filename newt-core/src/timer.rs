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
//!    marked in-flight (`claimed_at`) and stamped with a unique
//!    [`new_claim_token`], so a concurrent beat cannot re-fire it;
//! 2. **execute** it (`newt solve`, driven by `newt timer fire --run`);
//! 3. **acknowledge** ([`TimerStore::acknowledge`]) — presenting the claim
//!    token. On success a one-shot is removed and a repeating timer advances;
//!    on failure the claim is released and the timer stays pending/retryable.
//!    Execution stops at the first failure so later due timers are never
//!    silently lost — they remain pending (unclaimed) for the next beat.
//!
//! ## Why the token, and why the lock
//!
//! A claim goes stale after [`CLAIM_FRESH_SECS`] so a crashed beat cannot
//! strand a timer forever. That recovery is exactly what makes a *late* worker
//! dangerous: its timer may already belong to the beat that re-claimed it.
//! [`TimerStore::acknowledge`] therefore refuses any ack whose token is not the
//! one currently on the timer ([`AckOutcome::TokenMismatch`]) — a stalled
//! worker can no longer delete a one-shot another worker is still executing,
//! nor advance a repeat past a firing.
//!
//! Ownership is only meaningful if the claim itself is atomic, so every
//! mutation takes the queue's cross-process lock for its whole
//! read-modify-write and commits through [`crate::atomic_fs`]. A mutating read
//! that finds a corrupt file fails loudly instead of resolving to an empty
//! queue: the write that followed such a read would otherwise persist the
//! emptiness, destroying live timers and resetting the id sequence.
//!
//! The decision core ([`select_due`], [`select_claimable`], [`schedule_new`],
//! [`advance_repeat`], [`acknowledge_success`], [`acknowledge_failure`]) is
//! pure and fully unit-tested with an injected [`Clock`] — no real fs, no
//! wall-clock. The fs-touching [`TimerStore`] is a thin load/save wrapper
//! (same doctrine as `ocap_cmd`: the entry point is thin, the core is pure).
//!
//! Reuses [`crate::Config::user_config_dir`] for the root and plain JSON for
//! the envelope — *tamper*-evidence is not on the table for a prompt queue, so
//! the machinery of `store.rs` is deliberately not pulled in; durability and
//! mutual exclusion come from [`crate::atomic_fs`], which the queue does need.

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
    /// Unique opaque token set when claimed; the matching token is required to
    /// acknowledge. Prevents a stale worker from consuming a newer worker's
    /// claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_token: Option<String>,
    /// The workspace (CWD) where this timer should be executed. Makes the
    /// timer portable and explicit — the cron process CWD does NOT infer
    /// execution context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
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
    workspace: Option<PathBuf>,
) -> Timer {
    let seq = next_seq(timers);
    Timer {
        id: format!("tm_{seq}_{now}"),
        prompt: prompt.to_owned(),
        fire_at: now.saturating_add(after_secs),
        created_at: now,
        repeat_secs,
        claimed_at: None,
        claim_token: None,
        workspace,
    }
}

/// Mint an opaque, single-use claim token. Unique per claiming process and
/// call: `<pid>-<nanos>-<counter>`. The counter disambiguates two claims taken
/// inside the same nanosecond tick by one process.
#[must_use]
pub fn new_claim_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// The outcome of [`TimerStore::acknowledge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The claim matched and the timer was advanced/removed/released.
    Applied,
    /// No timer with that id exists.
    NotFound,
    /// The timer exists but is held under a *different* claim token — this
    /// worker no longer owns it (its claim went stale and another beat took
    /// over). The ack is refused; the current owner's claim is untouched.
    TokenMismatch,
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
            next.claim_token = None;
            return Some(next);
        }
    };
    next.fire_at = next.fire_at.saturating_add(advance);
    next.claimed_at = None;
    next.claim_token = None;
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
    t.claim_token = None;
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
        match self.load_strict() {
            Ok(timers) => Ok(timers),
            Err(e) => {
                tracing::warn!("timers.json unreadable, treating as empty: {e}");
                Ok(Vec::new())
            }
        }
    }

    /// Load for a **mutating** caller. Unlike [`Self::load`], a corrupt file is
    /// an error, never an empty queue.
    ///
    /// This distinction is the queue's durability guarantee. Every mutation is
    /// a read-modify-write; if a torn or corrupt read silently yielded an empty
    /// vec, the write that followed would persist that emptiness and destroy
    /// every live timer — and reset the id sequence so fresh ids collide with
    /// timers that still exist. Reading may degrade; writing must not.
    ///
    /// # Errors
    /// IO errors other than `NotFound`, and JSON parse failures.
    fn load_strict(&self) -> anyhow::Result<Vec<Timer>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<Vec<Timer>>(&bytes).map_err(|e| {
                anyhow::anyhow!(
                    "{} is corrupt ({e}); refusing to overwrite it. \
                     Move it aside to start a fresh queue.",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Take the cross-process lock guarding the queue file. Every
    /// read-modify-write below holds this for its whole critical section, so
    /// two concurrent `newt timer fire` beats cannot interleave and hand the
    /// same timer to two workers.
    fn lock(&self) -> anyhow::Result<crate::atomic_fs::LockGuard> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic_fs::acquire_lock(&crate::atomic_fs::stable_lock_path_for(&self.path)?)
    }

    /// Durably replace the queue file. Atomic (stage + rename), so a concurrent
    /// reader sees either the old queue or the new one — never a truncated file.
    fn save(&self, timers: &[Timer]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(timers)?;
        crate::atomic_fs::atomic_write(&self.path, &bytes)
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
        workspace: Option<PathBuf>,
    ) -> anyhow::Result<Timer> {
        // Config/persistence boundary: a zero repeat interval would make
        // `advance_repeat` spin forever once the job is due. Reject it here so
        // no caller — not just the CLI — can persist one.
        if let Some(0) = repeat_secs {
            anyhow::bail!("repeat interval must be greater than zero");
        }
        let _guard = self.lock()?;
        let now = clock.now_secs();
        let mut timers = self.load_strict()?;
        let timer = schedule_new(&timers, after_secs, prompt, now, repeat_secs, workspace);
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
        let _guard = self.lock()?;
        let now = clock.now_secs();
        let mut timers = self.load_strict()?;
        let pick = select_claimable(&timers, now)
            .into_iter()
            .min_by_key(|t| (t.created_at, t.fire_at));
        let Some(pick) = pick else {
            return Ok(None);
        };
        let target = pick.id.clone();
        let token = new_claim_token();
        for t in &mut timers {
            if t.id == target {
                t.claimed_at = Some(now);
                t.claim_token = Some(token.clone());
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
    pub fn acknowledge(
        &self,
        id: &str,
        token: &str,
        success: bool,
        clock: &dyn Clock,
    ) -> anyhow::Result<AckOutcome> {
        let _guard = self.lock()?;
        let now = clock.now_secs();
        let timers = self.load_strict()?;

        // Ownership check BEFORE any mutation: a worker whose claim went stale
        // (another beat re-claimed the timer under a new token) must not
        // consume, advance, or release the claim it no longer holds.
        let Some(current) = timers.iter().find(|t| t.id == id) else {
            return Ok(AckOutcome::NotFound);
        };
        if current.claim_token.as_deref() != Some(token) {
            return Ok(AckOutcome::TokenMismatch);
        }

        let mut kept: Vec<Timer> = Vec::with_capacity(timers.len());
        let mut done = false;
        for t in timers {
            if !done && t.id == id {
                done = true;
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
        Ok(AckOutcome::Applied)
    }

    /// Remove a timer by id (or an unambiguous id prefix) — cancel a watch.
    /// Returns `true` if exactly one timer matched and was removed.
    ///
    /// # Errors
    /// Propagates load/save IO errors.
    pub fn dismiss(&self, id_or_prefix: &str) -> anyhow::Result<bool> {
        let _guard = self.lock()?;
        let timers = self.load_strict()?;
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
            claim_token: None,
            workspace: None,
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
        let timer = schedule_new(&existing, 300, "check ci", now, None, None);
        assert_eq!(timer.fire_at, 1300);
        assert_eq!(timer.created_at, now);
        assert_eq!(timer.id, "tm_2_1000");
        assert!(timer.prompt.contains("check ci"));
        assert!(timer.claimed_at.is_none());
        // repeat carries through
        let rep = schedule_new(&existing, 60, "p", now, Some(300), None);
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
        let timer = schedule_new(&[], u64::MAX, "p", 1, None, None);
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
        let new = schedule_new(&timers, 10, "d", 5, None, None);
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
            claim_token: None,
            workspace: None,
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

    // ---------- #1747 fix-forward regressions ----------
    // Each of these FAILED (or asserted the opposite) against 828b846f, where
    // `claim_token` and `workspace` were declared on `Timer` but never set,
    // read, or enforced, and every mutation was an unlocked, non-atomic
    // read-modify-write over a load that turned corruption into an empty queue.

    struct Fixed(u64);
    impl Clock for Fixed {
        fn now_secs(&self) -> u64 {
            self.0
        }
    }

    /// Scratch dir unique per test; removed on drop even if the test panics.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "newt-timer-{tag}-{}-{}",
                std::process::id(),
                new_claim_token()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("scratch");
            Self(d)
        }
        fn store(&self) -> TimerStore {
            TimerStore::open(Some(&self.0)).expect("open")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Regression (#1747): a worker whose claim went stale must NOT be able to
    /// acknowledge — the timer now belongs to whichever beat re-claimed it.
    ///
    /// Before the fix `acknowledge` took no token and checked none, so worker
    /// A's late success deleted the one-shot that worker B was still
    /// executing: the wake ran twice and the queue lost the timer mid-flight.
    #[test]
    fn stale_worker_cannot_acknowledge_a_reclaimed_timer() {
        let s = Scratch::new("stale");
        let store = s.store();
        store.schedule(0, "wake", &Fixed(1000), None, None).unwrap();

        let a = store.claim_next_due(&Fixed(1000)).unwrap().unwrap();
        let a_token = a.claim_token.clone().expect("claim mints a token");

        // A hangs past CLAIM_FRESH_SECS; B takes over the now-stale claim.
        let t2 = 1000 + CLAIM_FRESH_SECS;
        let b = store.claim_next_due(&Fixed(t2)).unwrap().unwrap();
        let b_token = b.claim_token.clone().expect("re-claim mints a token");
        assert_eq!(a.id, b.id, "B re-claimed the same timer");
        assert_ne!(a_token, b_token, "each claim gets a distinct token");

        // A's late ack is refused, and leaves B's claim untouched.
        assert_eq!(
            store
                .acknowledge(&a.id, &a_token, true, &Fixed(t2 + 1))
                .unwrap(),
            AckOutcome::TokenMismatch,
        );
        let live = store.list().unwrap();
        assert_eq!(live.len(), 1, "B's in-flight timer survived A's late ack");
        assert_eq!(live[0].claim_token.as_deref(), Some(b_token.as_str()));

        // B, the real owner, can still complete the lifecycle.
        assert_eq!(
            store
                .acknowledge(&b.id, &b_token, true, &Fixed(t2 + 2))
                .unwrap(),
            AckOutcome::Applied,
        );
        assert!(store.list().unwrap().is_empty());
    }

    /// Regression (#1747): acknowledging an unknown id is distinguishable from
    /// acknowledging one this worker does not own.
    #[test]
    fn acknowledge_reports_not_found_separately_from_token_mismatch() {
        let s = Scratch::new("notfound");
        let store = s.store();
        assert_eq!(
            store.acknowledge("tm_9_9", "tok", true, &Fixed(1)).unwrap(),
            AckOutcome::NotFound,
        );
        store.schedule(0, "p", &Fixed(1000), None, None).unwrap();
        let t = store.claim_next_due(&Fixed(1000)).unwrap().unwrap();
        assert_eq!(
            store
                .acknowledge(&t.id, "not-the-token", true, &Fixed(1001))
                .unwrap(),
            AckOutcome::TokenMismatch,
        );
        // An unclaimed timer cannot be acknowledged by anyone.
        store
            .acknowledge(
                &t.id,
                t.claim_token.as_deref().unwrap(),
                false,
                &Fixed(1002),
            )
            .unwrap();
        assert_eq!(
            store
                .acknowledge(&t.id, "anything", true, &Fixed(1003))
                .unwrap(),
            AckOutcome::TokenMismatch,
            "a released timer is owned by nobody",
        );
    }

    /// Regression (#1747): a failed execution releases BOTH the claim stamp and
    /// the token, so the next beat can genuinely re-claim it.
    #[test]
    fn failed_ack_releases_claim_and_token() {
        let s = Scratch::new("failed");
        let store = s.store();
        store.schedule(0, "p", &Fixed(1000), None, None).unwrap();
        let a = store.claim_next_due(&Fixed(1000)).unwrap().unwrap();
        let tok = a.claim_token.clone().unwrap();
        assert_eq!(
            store.acknowledge(&a.id, &tok, false, &Fixed(1001)).unwrap(),
            AckOutcome::Applied,
        );
        let live = store.list().unwrap();
        assert_eq!(live[0].fire_at, 1000, "retryable: fire_at unchanged");
        assert!(live[0].claimed_at.is_none() && live[0].claim_token.is_none());
        // Immediately re-claimable — not blocked for CLAIM_FRESH_SECS.
        let b = store.claim_next_due(&Fixed(1002)).unwrap().unwrap();
        assert_ne!(b.claim_token, Some(tok));
    }

    /// Regression (#1747): the workspace is captured at schedule time and
    /// round-trips through persistence, so the firing host does not infer the
    /// execution directory from the cron process CWD.
    #[test]
    fn schedule_captures_and_persists_the_workspace() {
        let s = Scratch::new("ws");
        let store = s.store();
        let ws = PathBuf::from("/srv/project-x");
        let t = store
            .schedule(60, "build", &Fixed(1000), None, Some(ws.clone()))
            .unwrap();
        assert_eq!(t.workspace.as_ref(), Some(&ws));
        // Survives a reload (serde round-trip), and survives a repeat advance.
        assert_eq!(store.list().unwrap()[0].workspace.as_ref(), Some(&ws));
        let rep = store
            .schedule(0, "watch", &Fixed(1000), Some(60), Some(ws.clone()))
            .unwrap();
        let claimed = store.claim_next_due(&Fixed(2000)).unwrap().unwrap();
        assert_eq!(claimed.id, rep.id);
        store
            .acknowledge(
                &rep.id,
                claimed.claim_token.as_deref().unwrap(),
                true,
                &Fixed(2000),
            )
            .unwrap();
        let after = store.list().unwrap();
        let advanced = after.iter().find(|t| t.id == rep.id).expect("re-armed");
        assert_eq!(
            advanced.workspace.as_ref(),
            Some(&ws),
            "workspace survives re-arm"
        );
    }

    /// Regression (#1747): a corrupt queue file must NOT be silently treated as
    /// empty by a mutating caller. Before the fix, the next `schedule` wrote
    /// that empty view back — destroying every live timer and resetting the id
    /// sequence so the new id collided with ids still in use.
    #[test]
    fn corrupt_queue_never_destroys_timers_on_write() {
        let s = Scratch::new("corrupt");
        let store = s.store();
        store
            .schedule(60, "first", &Fixed(1000), None, None)
            .unwrap();
        store
            .schedule(60, "second", &Fixed(1000), None, None)
            .unwrap();

        std::fs::write(store.path(), b"{ truncated").unwrap();

        let err = store
            .schedule(60, "third", &Fixed(1000), None, None)
            .expect_err("a mutation over a corrupt queue must fail loudly");
        assert!(err.to_string().contains("corrupt"), "{err}");

        // Every other mutating entry point refuses too — none may overwrite.
        assert!(store.claim_next_due(&Fixed(2000)).is_err());
        assert!(store
            .acknowledge("tm_1_1000", "t", true, &Fixed(2000))
            .is_err());
        assert!(store.dismiss("tm_1").is_err());

        // The bytes on disk are untouched: the queue is recoverable by hand.
        assert_eq!(std::fs::read(store.path()).unwrap(), b"{ truncated");

        // Read-only inspection still degrades gracefully rather than blocking.
        assert!(store.list().unwrap().is_empty());
    }

    /// Regression (#1747): ids stay unique across a full drain. `next_seq` is
    /// derived from the max live seq, so a queue that empties and refills in
    /// the same clock second must not re-mint a live id.
    #[test]
    fn ids_stay_unique_across_an_emptying_queue() {
        let s = Scratch::new("ids");
        let store = s.store();
        let a = store.schedule(0, "a", &Fixed(1000), None, None).unwrap();
        let claimed = store.claim_next_due(&Fixed(1000)).unwrap().unwrap();
        store
            .acknowledge(
                &a.id,
                claimed.claim_token.as_deref().unwrap(),
                true,
                &Fixed(1000),
            )
            .unwrap();
        assert!(store.list().unwrap().is_empty(), "queue drained");

        // Same second, empty store: a fresh id is minted. It is only safe for
        // it to reuse seq 1 because nothing live holds it.
        let b = store.schedule(0, "b", &Fixed(1000), None, None).unwrap();
        let c = store.schedule(0, "c", &Fixed(1000), None, None).unwrap();
        assert_ne!(b.id, c.id, "concurrent-second schedules never collide");
        assert_eq!(store.list().unwrap().len(), 2);
        // And a prefix dismiss stays unambiguous.
        assert!(store.dismiss(&c.id).unwrap());
    }

    /// Regression (#1747): two beats racing to claim the SAME due timer must
    /// hand it to exactly one worker. The queue mutation is lock-guarded, so
    /// the loser sees nothing claimable rather than a duplicate wake.
    #[test]
    fn concurrent_beats_never_hand_one_timer_to_two_workers() {
        let s = Scratch::new("race");
        let dir = s.0.clone();
        {
            let store = s.store();
            store
                .schedule(0, "only-once", &Fixed(1000), None, None)
                .unwrap();
        }
        let winners = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let dir = dir.clone();
            let winners = std::sync::Arc::clone(&winners);
            handles.push(std::thread::spawn(move || {
                let store = TimerStore::open(Some(&dir)).unwrap();
                if let Ok(Some(t)) = store.claim_next_due(&Fixed(1000)) {
                    winners.lock().unwrap().push(t.claim_token.unwrap());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let winners = winners.lock().unwrap();
        assert_eq!(
            winners.len(),
            1,
            "exactly one beat may claim a due timer, got {winners:?}"
        );
    }
}
