//! Operator steering: input submitted mid-turn, delivered at the next safe
//! agent-loop boundary.
//!
//! An operator watching a long turn must be able to correct it without waiting
//! for the round cap — and without corrupting an inference request already on
//! the wire or a tool already executing. So steering is *queued*, never
//! injected: [`SessionSteeringInbox::submit`] records the message, and the
//! agent loop drains it at the top of the next round, ahead of the next model
//! call, as a genuine operator user message.
//!
//! Three states are modelled explicitly, because collapsing them is what makes
//! an "edit my last instruction" feature lie:
//!
//! ```text
//! draft     text still in the editor — this module never sees it
//! queued    submitted, not yet read by the agent — editable, [`Rev`]-addressed
//! consumed  incorporated into a reasoning cycle — immutable, normal history
//! ```
//!
//! Editing a *queued* message replaces it in place, keeping its causal
//! position. Editing one the agent has already read is refused with
//! [`ReplaceRejected::Consumed`] rather than silently appending a second,
//! contradictory instruction — an optimistic "edited!" that was in fact
//! already sent is a correctness bug the operator would discover only from the
//! model's behavior.
//!
//! Steering is NOT cancellation. A tool already running finishes normally;
//! steering affects what the agent does *next*. Hard cancellation stays
//! `ChatCtx::cancel`.

use std::sync::Mutex;

/// Identifies one submitted steering message, so an edit can name exactly
/// which queued message it means to replace.
///
/// Monotonic per inbox. Every accepted [`SessionSteeringInbox::submit`] or
/// [`SessionSteeringInbox::replace`] mints a fresh one, so a stale handle can
/// never clobber a newer edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rev(u64);

impl Rev {
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Why an edit of a queued message was refused.
///
/// Both variants mean "the handle you hold no longer names a queued message",
/// but the operator needs to know *which* — one says the instruction landed,
/// the other says it was overtaken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceRejected {
    /// The agent already read this message. It is history now; a correction
    /// has to be submitted as a new instruction.
    Consumed,
    /// A newer edit replaced this revision before you submitted yours.
    Superseded,
}

impl ReplaceRejected {
    /// One line for the operator, naming what happened and what to do.
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            Self::Consumed => {
                "the agent already read that instruction — submit your correction as a new message"
            }
            Self::Superseded => {
                "a newer edit replaced that instruction — recall again to edit the current one"
            }
        }
    }
}

impl std::fmt::Display for ReplaceRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.explain())
    }
}

/// The agent loop's view of operator steering: drain whatever the operator has
/// submitted since the last round.
///
/// Deliberately narrower than [`SessionSteeringInbox`]'s full API — the loop
/// consumes, it never submits or edits. Injected exactly like
/// [`PlanModeControl`](super::plan_mode::PlanModeControl): `Send + Sync`,
/// `&self`, and `None` on [`ChatCtx`](super::ChatCtx) leaves every headless
/// caller bit-for-bit unchanged.
pub trait SteeringInbox: Send + Sync {
    /// Take every queued message, oldest first, marking them consumed.
    ///
    /// Called at the top of each tool round, before the next model call. An
    /// empty result must be the common case and must cost nothing.
    fn drain_for_round(&self) -> Vec<String>;
}

#[derive(Debug, Default)]
struct InboxState {
    /// Next revision to mint. Monotonic; never reused.
    next: u64,
    /// Queued messages in submission order — causal order is the contract.
    queued: Vec<(Rev, String)>,
    /// Highest revision ever drained, or `None` if nothing has been consumed
    /// yet. Distinguishes "consumed" from "superseded" without retaining every
    /// historical revision.
    ///
    /// `Option`, not a `0` sentinel: `Rev(0)` is a perfectly good revision —
    /// it is the FIRST one — so a bare counter reports the very first message
    /// as already-consumed before any drain has happened. Caught by
    /// `a_stale_handle_cannot_clobber_a_newer_edit`.
    consumed_through: Option<u64>,
}

/// One session's steering queue.
///
/// Owned by the session (an `Arc` beside its cancel flag), never process-wide:
/// tab A's steering must be unable to reach tab B's agent, and a shared static
/// would make that a matter of discipline rather than construction.
#[derive(Debug, Default)]
pub struct SessionSteeringInbox {
    state: Mutex<InboxState>,
}

impl SessionSteeringInbox {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InboxState> {
        // A poisoned steering queue must not take the session down; the
        // pending messages are recoverable state, not an invariant.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue an operator message for the next round. Returns its [`Rev`], the
    /// handle needed to edit it before the agent reads it.
    ///
    /// Whitespace-only submissions are still queued: the operator pressed
    /// Enter, and silently dropping that would leave the UI showing a steer
    /// that does not exist. Callers that want to ignore blank input should do
    /// so before calling.
    pub fn submit(&self, text: impl Into<String>) -> Rev {
        let mut st = self.lock();
        let rev = Rev(st.next);
        st.next += 1;
        st.queued.push((rev, text.into()));
        rev
    }

    /// Replace a still-queued message, atomically and in place.
    ///
    /// Keeps the message's causal position — an edit is a correction of that
    /// instruction, not a new one at the back of the queue — and mints a fresh
    /// [`Rev`] so a concurrently-held stale handle cannot overwrite this edit.
    ///
    /// # Errors
    /// [`ReplaceRejected::Consumed`] once the agent has read it,
    /// [`ReplaceRejected::Superseded`] if a newer edit got there first.
    pub fn replace(&self, rev: Rev, text: impl Into<String>) -> Result<Rev, ReplaceRejected> {
        let mut st = self.lock();
        match st.queued.iter().position(|(r, _)| *r == rev) {
            Some(idx) => {
                let fresh = Rev(st.next);
                st.next += 1;
                st.queued[idx] = (fresh, text.into());
                Ok(fresh)
            }
            // Not queued: either it was drained, or an edit already moved past
            // it. `consumed_through` separates the two without keeping a
            // record of every revision this session ever minted.
            None if matches!(st.consumed_through, Some(high) if rev.0 <= high) => {
                Err(ReplaceRejected::Consumed)
            }
            None => Err(ReplaceRejected::Superseded),
        }
    }

    /// The most recent still-queued message — what Up-arrow on an empty editor
    /// should recall, so the operator edits their pending instruction rather
    /// than starting a contradictory second one.
    ///
    /// `None` once everything has been consumed; the caller then falls back to
    /// ordinary history.
    #[must_use]
    pub fn recall_latest_unconsumed(&self) -> Option<(Rev, String)> {
        self.lock().queued.last().cloned()
    }

    /// How many messages are waiting — for the "· 2 steers queued" indicator.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.lock().queued.len()
    }

    /// Whether a given revision is still editable.
    #[must_use]
    pub fn is_queued(&self, rev: Rev) -> bool {
        self.lock().queued.iter().any(|(r, _)| *r == rev)
    }
}

impl SteeringInbox for SessionSteeringInbox {
    fn drain_for_round(&self) -> Vec<String> {
        let mut st = self.lock();
        if st.queued.is_empty() {
            return Vec::new();
        }
        let taken: Vec<(Rev, String)> = std::mem::take(&mut st.queued);
        if let Some((high, _)) = taken.last() {
            st.consumed_through = Some(high.0);
        }
        taken.into_iter().map(|(_, text)| text).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submitted_message_is_queued_until_the_loop_drains_it() {
        let inbox = SessionSteeringInbox::new();
        assert_eq!(inbox.pending(), 0);
        assert!(inbox.drain_for_round().is_empty());

        let rev = inbox.submit("don't change the public API");
        assert_eq!(inbox.pending(), 1);
        assert!(inbox.is_queued(rev));

        assert_eq!(
            inbox.drain_for_round(),
            vec!["don't change the public API".to_string()]
        );
        assert_eq!(inbox.pending(), 0);
        assert!(!inbox.is_queued(rev));
        // Draining twice must not re-deliver.
        assert!(inbox.drain_for_round().is_empty());
    }

    #[test]
    fn multiple_steers_retain_causal_order() {
        let inbox = SessionSteeringInbox::new();
        inbox.submit("first");
        inbox.submit("second");
        inbox.submit("third");
        assert_eq!(inbox.drain_for_round(), vec!["first", "second", "third"]);
    }

    #[test]
    fn an_unconsumed_message_can_be_recalled_and_replaced_in_place() {
        let inbox = SessionSteeringInbox::new();
        let rev = inbox.submit("don't change the public API");

        let (recalled_rev, text) = inbox.recall_latest_unconsumed().expect("still queued");
        assert_eq!(recalled_rev, rev);
        assert_eq!(text, "don't change the public API");

        let fresh = inbox
            .replace(
                rev,
                "don't change the public API; add a compatibility shim instead",
            )
            .expect("still editable");
        assert_ne!(fresh, rev, "an edit mints a fresh revision");

        // ONE message reaches the agent, not two contradictory ones.
        assert_eq!(inbox.pending(), 1);
        assert_eq!(
            inbox.drain_for_round(),
            vec!["don't change the public API; add a compatibility shim instead".to_string()]
        );
    }

    #[test]
    fn an_edit_keeps_its_causal_position_rather_than_moving_to_the_back() {
        let inbox = SessionSteeringInbox::new();
        let first = inbox.submit("first");
        inbox.submit("second");
        inbox.replace(first, "first (corrected)").expect("queued");
        assert_eq!(
            inbox.drain_for_round(),
            vec!["first (corrected)", "second"],
            "an edit corrects that instruction; it does not become the newest one"
        );
    }

    #[test]
    fn a_consumed_message_cannot_be_silently_rewritten() {
        let inbox = SessionSteeringInbox::new();
        let rev = inbox.submit("don't change the public API");
        assert_eq!(inbox.drain_for_round().len(), 1);

        // The agent has read it. The edit must be REFUSED, not appended as a
        // second instruction the operator never intended to send twice.
        assert_eq!(
            inbox.replace(rev, "actually, do change it"),
            Err(ReplaceRejected::Consumed)
        );
        assert_eq!(inbox.pending(), 0, "a refused edit queues nothing");
        assert!(inbox.recall_latest_unconsumed().is_none());
    }

    #[test]
    fn a_stale_handle_cannot_clobber_a_newer_edit() {
        let inbox = SessionSteeringInbox::new();
        let first = inbox.submit("v1");
        let second = inbox.replace(first, "v2").expect("queued");

        // The losing side of the race is told it was overtaken — a different
        // fact from "already sent", and the operator needs to know which.
        assert_eq!(
            inbox.replace(first, "v1 edited"),
            Err(ReplaceRejected::Superseded)
        );
        assert!(inbox.is_queued(second));
        assert_eq!(inbox.drain_for_round(), vec!["v2"]);
    }

    /// Regression: the FIRST revision is `Rev(0)`, and a bare `u64`
    /// high-water mark reports it as consumed before anything has been
    /// drained — `0 <= 0`. The distinction has to survive at the boundary or
    /// the operator gets told "already sent" about a message still sitting in
    /// the queue.
    #[test]
    fn the_very_first_revision_is_not_born_consumed() {
        let inbox = SessionSteeringInbox::new();
        let first = inbox.submit("v1");
        assert_eq!(first.as_u64(), 0, "fixture pins the boundary revision");
        inbox.replace(first, "v2").expect("nothing drained yet");

        // Superseded, not Consumed — no drain has happened.
        assert_eq!(inbox.replace(first, "v3"), Err(ReplaceRejected::Superseded));
    }

    #[test]
    fn the_two_refusals_say_different_things() {
        assert_ne!(
            ReplaceRejected::Consumed.explain(),
            ReplaceRejected::Superseded.explain()
        );
        assert!(ReplaceRejected::Consumed.explain().contains("already read"));
        assert!(ReplaceRejected::Superseded.explain().contains("newer edit"));
    }

    #[test]
    fn recall_prefers_the_most_recent_unconsumed_message() {
        let inbox = SessionSteeringInbox::new();
        inbox.submit("older");
        let newer = inbox.submit("newer");
        assert_eq!(
            inbox.recall_latest_unconsumed(),
            Some((newer, "newer".to_string()))
        );
    }

    #[test]
    fn steering_submitted_after_a_drain_is_a_new_instruction() {
        let inbox = SessionSteeringInbox::new();
        inbox.submit("round one steer");
        assert_eq!(inbox.drain_for_round().len(), 1);

        let rev = inbox.submit("round two steer");
        assert!(
            inbox.is_queued(rev),
            "post-drain submissions queue normally"
        );
        assert_eq!(inbox.drain_for_round(), vec!["round two steer"]);
    }

    #[test]
    fn the_inbox_is_shareable_across_threads_and_stays_ordered() {
        use std::sync::Arc;
        // The UI thread submits while a session thread drains — the whole
        // reason this is a Mutex rather than the cancel flag's AtomicBool.
        let inbox = Arc::new(SessionSteeringInbox::new());
        let writer = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                for i in 0..50 {
                    inbox.submit(format!("steer-{i}"));
                }
            })
        };
        writer.join().expect("writer");

        let drained = inbox.drain_for_round();
        assert_eq!(drained.len(), 50);
        for (i, text) in drained.iter().enumerate() {
            assert_eq!(text, &format!("steer-{i}"), "submission order preserved");
        }
    }

    #[test]
    fn the_trait_object_only_exposes_draining() {
        // The loop gets exactly one verb. Submitting and editing stay with the
        // session that owns the queue.
        let inbox = SessionSteeringInbox::new();
        inbox.submit("via the concrete type");
        let as_trait: &dyn SteeringInbox = &inbox;
        assert_eq!(as_trait.drain_for_round(), vec!["via the concrete type"]);
    }
}
