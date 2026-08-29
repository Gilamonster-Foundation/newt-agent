//! **The typed progress/lifecycle contract and its renderer-neutral sink**
//! (D2a, #1864).
//!
//! # Why this is not the interaction path
//!
//! A2/A3 gave interactions a request/reply shape: a definition is published, a
//! human answers, exactly one response resolves it. That is a **blocking**
//! contract, and it is the right one for a question.
//!
//! Progress is the opposite shape. A spinner tick is not an offer: nobody
//! replies to it, it carries no authority, it arrives ten times a second, and
//! dropping one costs nothing. Routing it through the interaction controller
//! would put a high-rate stream down a path built to block on a human and to
//! resolve exactly once. **Keeping them separate is the structural point of
//! this slice**, so this module names no interaction type and takes no
//! dependency on `newt_interaction`.
//!
//! # The scrollback boundary is a TYPE, not a flag
//!
//! The rule is:
//!
//! > **Transient animation is view state. It never reaches committed
//! > scrollback.**
//!
//! A first cut made that a rule about an enum — one `Event` type with a
//! `Frame` variant beside a `Snapshot` variant — and left "pick the right one"
//! to the caller. That is the same shape as a boolean flag, and a flag gets
//! passed wrong eventually. So the two are **different types**:
//!
//! - [`Frame`] is transient. There is **no function anywhere from [`Frame`] to
//!   [`Commit`]** — not a discouraged one, not one behind an argument. The
//!   compiler is the guard; the behavioural test is belt-and-braces on a
//!   property the type already makes unrepresentable.
//! - [`Durable`] is everything that may persist, and [`Scrollback::offer`]
//!   accepts only it.
//!
//! What a [`Durable`] commits is then a smaller, honest question:
//!
//! | [`Durable`] | commits? | why |
//! |---|---|---|
//! | [`Durable::Started`] | yes | a lifecycle fact, true forever after |
//! | [`Durable::Snapshot`] | only if it **advances** | a repeat or a regression adds nothing |
//! | [`Durable::Note`] | yes | the harness speaking; routed through the ONE [`Notice`] seam |
//! | [`Durable::Finished`] | yes, and **seals** | terminal state; nothing commits after it |
//!
//! # The notice/status semantics table
//!
//! ONE table, so each family's cutover PR has a single thing to collapse onto
//! rather than re-deciding the mapping. Every row is a family that emits
//! progress or status **today**; nothing here is switched on by D2a.
//!
//! | family (today) | where | becomes |
//! |---|---|---|
//! | `print_newt` / `newt_line` narration | `agentic::display` | [`Durable::Note`] `{ Level::Info, "▸" }` |
//! | `print_harness_notice` (the amber "newt:" register) | `agentic::display` | [`Durable::Note`] `{ Level::Warn, "⚠" }` |
//! | [`Notice`] emitted directly | `tty::widgets::notice` | [`Durable::Note`] with that same [`Level`] + glyph |
//! | `Spinner` frames + stage labels | `tty::spinner` | [`Frame`] |
//! | `Spinner` char/step counters | `tty::spinner` | [`Durable::Snapshot`] |
//! | `lifecycle::LifecycleEvent` turn facts | `newt_core::lifecycle` | [`Durable::Started`] / [`Durable::Finished`], via [`from_lifecycle`] |
//! | `lifecycle::LifecycleEvent::Thinking` / `ToolActivity` | `newt_core::lifecycle` | [`Frame`] — a caller's choice, never automatic |
//!
//! Two rows are deliberately strict. A spinner's *stage label* is a [`Frame`]
//! even though it changes rarely — it is view state, and "rarely" is not a
//! semantic category. And a counter is a [`Durable::Snapshot`] even though the
//! spinner renders it inside an animated row: the same number is transient in
//! the row and durable in scrollback, which is exactly the distinction this
//! contract carries.
//!
//! # This does not compete with `lifecycle` — it reads from it
//!
//! `newt_core::lifecycle` already owns the vocabulary for what a turn is doing
//! (`TurnStarted`, `TurnCompleted`, `TurnFailed`, `TurnCancelled`), with the
//! same push/inline/unbuffered delivery, already emitted by `newt-tui` and
//! already consumed by `herdr`. Publishing a second set of those facts would
//! be the third abstraction D0's gate forbids — so [`from_lifecycle`] adapts
//! the existing events instead, and nothing here emits.
//!
//! What this module adds is what `lifecycle` has no vocabulary for and should
//! not grow one for: a [`Measure`] and its monotonicity, a [`Note`]'s level and
//! glyph, [`Frame`]'s transience, and the commit boundary between them.
//!
//! # Push, synchronous, and unbuffered — on purpose
//!
//! The sink is **push**: a producer calls it inline. There is no channel, no
//! queue, and no background drain thread anywhere in this module.
//!
//! That is a deliberate answer to the failure mode a high-rate producer
//! invites. An unbounded channel between a spinner ticking ten times a second
//! and a consumer that is momentarily slow does not fail loudly — it grows,
//! and a long turn quietly becomes a memory leak. A bounded channel trades
//! that for a different question (block the producer, or drop?) that the
//! producer is in no position to answer.
//!
//! Push settles it by construction: memory is whatever the sink chooses to
//! keep, and the sink knows what it is for. The two shipped answers:
//!
//! - [`Scrollback`] keeps only what **committed** — bounded by the monotone
//!   story, not by the frame rate. A million frames add nothing to it.
//! - [`LatestFrame`] keeps exactly **one** frame, the most recent. Coalescing
//!   is the right drop policy for animation: an operator wants the current
//!   state, never a backlog of stale ones.
//!
//! [`ProgressSink::frame`] is therefore documented *may drop, must not block*,
//! and every sink here is O(1) in the number of frames.
//!
//! # No ambient clock
//!
//! Events carry `at_ms`, supplied by the producer. This module never reads a
//! clock, so a test states time instead of waiting for it and no assertion
//! here is saturation-fragile. (`timer::Clock` is deliberately not reused: it
//! is second-resolution and belongs to the timer queue.)
//!
//! # Dual-publish
//!
//! Nothing here is authoritative. D2a publishes **beside** the existing
//! stdout/`TurnDriver` paths, which stay in charge; each family's cutover is
//! its own later PR with its own deletion gate. A sink that silently became
//! the real output path would produce exactly the "old plus new" end state the
//! refactor must not reach.
//!
//! [`Notice`]: crate::tty::widgets::Notice

use crate::tty::widgets::Level;

/// Which unit of work an event is about.
///
/// A borrowed `&'static str` on purpose: task identity is a program constant
/// ("compaction", "retrieval"), not operator data, so this allocates nothing
/// on a path that runs ten times a second.
pub type TaskId = &'static str;

/// How far along a unit of work is.
///
/// `total: None` is honest about work whose size is unknown — the common case
/// for a model turn. It is not a synonym for zero.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Measure {
    /// Units finished. The only field monotonicity is judged on.
    pub done: u64,
    /// Units expected, when that is knowable.
    pub total: Option<u64>,
}

impl Measure {
    /// A measure of `done` units out of an unknown total.
    #[must_use]
    pub fn of(done: u64) -> Self {
        Self { done, total: None }
    }

    /// A measure of `done` out of `total`.
    #[must_use]
    pub fn out_of(done: u64, total: u64) -> Self {
        Self {
            done,
            total: Some(total),
        }
    }
}

/// How a unit of work ended.
///
/// Every variant is terminal. There is deliberately no `Paused`: a resumable
/// task starts a new run rather than reopening a sealed one, which is what
/// keeps "nothing commits after the end" checkable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Finished doing what it set out to do.
    Completed,
    /// Stopped because something went wrong.
    Failed,
    /// Stopped because the operator asked.
    Cancelled,
}

/// A harness notice, in the vocabulary the ONE [`Notice`] widget already
/// speaks.
///
/// Carries [`Level`] and the glyph rather than a second hue table: D0's
/// standing deletion gate forbids a third abstraction, and `Notice` is the
/// seam to promote, not to sit beside.
///
/// [`Notice`]: crate::tty::widgets::Notice
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The register, from the ONE hue table.
    pub level: Level,
    /// The meaning-carrying sigil. Never decoration — see `notice.rs`.
    pub glyph: &'static str,
    /// The text.
    pub text: String,
}

/// **Transient view state.** One tick of animation.
///
/// This type is the scrollback boundary. It has no conversion to [`Commit`],
/// [`Scrollback::offer`] will not accept it, and nothing in this module or its
/// renderer can turn one into a committed line. A spinner frame in an
/// operator's history is a defect, so the defect is not representable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// What the ephemeral row should currently read.
    pub label: String,
}

impl Frame {
    /// A frame showing `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

/// **Durable progress.** Everything that may reach committed scrollback.
///
/// Separate from [`Frame`] by type, not by discriminant: see the module docs
/// on the scrollback boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Durable {
    /// Work began.
    Started {
        /// Human-readable label, e.g. `"compacting context"`.
        label: String,
    },
    /// A measurement of how far along the work is.
    Snapshot(Measure),
    /// The harness saying something durable about the work.
    Note(Note),
    /// Work ended. Terminal: nothing commits after this.
    Finished(Outcome),
}

/// A renderer-neutral consumer of progress.
///
/// Object-safe so a caller can hold `&mut dyn ProgressSink`, and deliberately
/// infallible: a consumer that can fail gives every producer an error to
/// handle on a path that runs ten times a second, and the only honest recovery
/// is to drop the event anyway.
///
/// The two methods are the boundary. A producer cannot route animation into
/// the durable path by passing a different argument — it would have to build a
/// [`Durable`], which means choosing what the thing actually *means*.
pub trait ProgressSink: Send {
    /// Publish one animation frame.
    ///
    /// **May drop; must not block.** Called at frame rate; a sink that cannot
    /// keep up coalesces or discards, and must never buffer without bound. See
    /// the module docs on push/unbuffered.
    fn frame(&mut self, task: TaskId, at_ms: u64, frame: &Frame);

    /// Publish one durable event. Must not block on a human, must not fail.
    fn record(&mut self, task: TaskId, at_ms: u64, event: &Durable);
}

/// A sink that discards everything.
///
/// The headless/eval default, and what a `None` progress consumer collapses to
/// so producers need no `Option` dance.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl ProgressSink for NullSink {
    fn frame(&mut self, _task: TaskId, _at_ms: u64, _frame: &Frame) {}
    fn record(&mut self, _task: TaskId, _at_ms: u64, _event: &Durable) {}
}

/// Keeps exactly the most recent frame and nothing else.
///
/// The bounded answer to a high-rate producer: coalescing, O(1) in memory
/// however many frames arrive. An operator wants the *current* state of an
/// animation, never a backlog of stale ones, so dropping the older frame is
/// not a compromise — it is the correct semantics.
#[derive(Debug, Default, Clone)]
pub struct LatestFrame {
    latest: Option<(TaskId, u64, Frame)>,
}

impl LatestFrame {
    /// A fresh, empty holder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent frame, if any: `(task, at_ms, frame)`.
    #[must_use]
    pub fn peek(&self) -> Option<&(TaskId, u64, Frame)> {
        self.latest.as_ref()
    }
}

impl ProgressSink for LatestFrame {
    fn frame(&mut self, task: TaskId, at_ms: u64, frame: &Frame) {
        self.latest = Some((task, at_ms, frame.clone()));
    }

    /// Durable events are not this sink's business — it holds view state only.
    fn record(&mut self, _task: TaskId, _at_ms: u64, _event: &Durable) {}
}

/// What a [`Durable`] contributed to committed scrollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Commit {
    /// The work began.
    Started {
        /// The label it began with.
        label: String,
    },
    /// The work advanced to here.
    Advanced(Measure),
    /// A harness notice.
    Note(Note),
    /// The work ended.
    Finished(Outcome),
}

/// The accumulated committed story of one unit of work.
///
/// Feed it every [`Durable`] a producer emits and what comes out is what an
/// operator's scrollback is allowed to contain. It is **bounded by the
/// monotone story, not by the event rate**: repeats, regressions and
/// everything after the ending are dropped rather than stored.
///
/// It cannot be fed a [`Frame`] — that is a type error, not a runtime check.
#[derive(Debug, Default, Clone)]
pub struct Scrollback {
    committed: Vec<Commit>,
    /// Highest `done` committed so far — the monotonicity floor.
    high_water: Option<u64>,
    /// Set by [`Durable::Finished`]. Once sealed, nothing else commits.
    sealed: bool,
}

impl Scrollback {
    /// A fresh, empty scrollback.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a durable event. Returns what it committed, if anything.
    ///
    /// Returning the commit (rather than only recording it) lets a renderer be
    /// a thin adapter: it commits exactly when this says to, so "what
    /// scrolled" and "what this recorded" cannot drift.
    pub fn offer(&mut self, event: &Durable) -> Option<Commit> {
        let commit = self.commit_of(event)?;
        if matches!(commit, Commit::Finished(_)) {
            self.sealed = true;
        }
        if let Commit::Advanced(m) = &commit {
            self.high_water = Some(m.done);
        }
        self.committed.push(commit.clone());
        Some(commit)
    }

    /// The rule, with this scrollback's history as context.
    fn commit_of(&self, event: &Durable) -> Option<Commit> {
        // Terminal state seals the record. A late snapshot is late, not new —
        // committing it would let scrollback contradict its own last line.
        if self.sealed {
            return None;
        }
        match event {
            Durable::Started { label } => Some(Commit::Started {
                label: label.clone(),
            }),
            Durable::Snapshot(m) => match self.high_water {
                // A repeat or a regression adds nothing an operator can act on.
                Some(hw) if m.done <= hw => None,
                _ => Some(Commit::Advanced(*m)),
            },
            Durable::Note(n) => Some(Commit::Note(n.clone())),
            Durable::Finished(o) => Some(Commit::Finished(*o)),
        }
    }

    /// Everything committed, in order.
    #[must_use]
    pub fn committed(&self) -> &[Commit] {
        &self.committed
    }

    /// Whether terminal state has been recorded.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
}

/// Translate an existing [`LifecycleEvent`] into the durable progress it
/// implies, or `None` when it implies none.
///
/// **This is the dual-publish seam, and it is an adapter on purpose.**
///
/// `newt_core::lifecycle` already owns the vocabulary for what a turn is doing
/// — `TurnStarted`, `TurnCompleted`, `TurnFailed`, `TurnCancelled` — with the
/// same push/inline/unbuffered delivery this module chose, and `newt-tui`
/// already emits it and `herdr` already consumes it. A first cut of D2a had
/// `TurnDriver` publish its own `Started`/`Finished` beside all that, which is
/// a **second vocabulary for facts that already have one** — exactly the third
/// abstraction D0's deletion gate forbids. It was reverted.
///
/// So progress does not compete with the lifecycle seam; it reads from it. An
/// integration subscribes where integrations already subscribe, maps through
/// here, and feeds a [`ProgressSink`]. Nothing new emits, nothing changes what
/// an existing observer sees, and the contract is still exercised by real
/// events — which is what dual-publish is supposed to mean.
///
/// What this module adds *on top of* lifecycle is what lifecycle has no
/// vocabulary for and should not grow one for: a [`Measure`] and its
/// monotonicity, a [`Note`]'s level and glyph, [`Frame`]'s transience, and the
/// commit boundary that separates the last two.
///
/// [`LifecycleEvent`]: crate::lifecycle::LifecycleEvent
#[must_use]
pub fn from_lifecycle(event: &crate::lifecycle::LifecycleEvent) -> Option<Durable> {
    use crate::lifecycle::LifecycleEvent as L;
    match event {
        L::TurnStarted => Some(Durable::Started {
            label: "turn".to_string(),
        }),
        L::TurnCompleted => Some(Durable::Finished(Outcome::Completed)),
        L::TurnFailed { .. } => Some(Durable::Finished(Outcome::Failed)),
        L::TurnCancelled => Some(Durable::Finished(Outcome::Cancelled)),
        // Everything else is either session-scoped bookkeeping or a state a
        // renderer shows transiently. `Thinking` and `ToolActivity` in
        // particular are FRAME material, not durable: committing a line every
        // time the agent starts thinking is the scrollback noise this contract
        // exists to prevent. A caller that wants them animated builds a
        // `Frame`; this adapter will not decide that for it.
        L::SessionStarted { .. }
        | L::Waiting
        | L::Thinking
        | L::ToolActivity { .. }
        | L::Blocked
        | L::Unblocked
        | L::SessionEnded => None,
        // Deliberately exhaustive, with no catch-all. `LifecycleEvent` is
        // `#[non_exhaustive]`, but that binds only downstream crates — inside
        // `newt-core` this match is total, so adding a variant BREAKS THE BUILD
        // here and whoever adds it has to decide whether it is durable. A
        // `_ => None` would have made every future variant silently transient,
        // which is the wrong default to pick on someone else's behalf.
    }
}

/// Recording a stream is itself a sink: this is the observation-only consumer
/// D2a dual-publishes into.
///
/// It emits **nothing**, and its [`frame`](ProgressSink::frame) is a genuine
/// no-op — not "a no-op today". That is the point of the slice: the events
/// flow, the contract is exercised, and the operator still sees exactly the
/// bytes the old path produced.
impl ProgressSink for Scrollback {
    /// Frames are view state. This is the type-level boundary showing up at
    /// runtime: there is nothing this method *could* do with a [`Frame`] that
    /// would reach [`committed`](Self::committed).
    fn frame(&mut self, _task: TaskId, _at_ms: u64, _frame: &Frame) {}

    fn record(&mut self, _task: TaskId, _at_ms: u64, event: &Durable) {
        let _ = self.offer(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: TaskId = "compaction";

    fn sb_offer(sb: &mut Scrollback, d: Durable) -> Option<Commit> {
        sb.offer(&d)
    }

    /// **The boundary, behaviourally.** Belt-and-braces: the type already makes
    /// this unrepresentable (there is no `Frame -> Commit` anywhere), so this
    /// asserts the runtime path agrees with the type.
    ///
    /// Anti-vacuous twin: `and_a_durable_in_the_same_stream_does_commit`.
    #[test]
    fn frames_have_no_path_to_scrollback() {
        let mut sb = Scrollback::new();
        for t in 0..500 {
            sb.frame(TASK, t, &Frame::new(format!("working {t}")));
        }
        assert!(
            sb.committed().is_empty(),
            "500 frames committed {:?}",
            sb.committed()
        );
    }

    /// The twin. Without it, the test above would pass on a `Scrollback` that
    /// commits nothing at all.
    #[test]
    fn and_a_durable_in_the_same_stream_does_commit() {
        let mut sb = Scrollback::new();
        for t in 0..50 {
            sb.frame(TASK, t, &Frame::new("spin"));
        }
        sb.record(TASK, 50, &Durable::Snapshot(Measure::of(1)));
        assert_eq!(
            sb.committed(),
            &[Commit::Advanced(Measure::of(1))],
            "a real measurement must still get through the same sink"
        );
    }

    /// Only *advancing* snapshots commit: a repeat or a regression adds
    /// nothing an operator can act on.
    #[test]
    fn only_advancing_snapshots_commit() {
        let mut sb = Scrollback::new();
        assert!(sb_offer(&mut sb, Durable::Snapshot(Measure::of(5))).is_some());
        assert_eq!(
            sb_offer(&mut sb, Durable::Snapshot(Measure::of(5))),
            None,
            "a repeat of the same measurement is not progress"
        );
        assert_eq!(
            sb_offer(&mut sb, Durable::Snapshot(Measure::of(3))),
            None,
            "a regression must never commit — scrollback cannot go backwards"
        );
        assert!(
            sb_offer(&mut sb, Durable::Snapshot(Measure::of(6))).is_some(),
            "and the next genuine advance still commits"
        );
    }

    /// Terminal state seals the record: nothing appends after the ending.
    #[test]
    fn nothing_commits_after_terminal_state() {
        let mut sb = Scrollback::new();
        sb_offer(&mut sb, Durable::Finished(Outcome::Completed));
        assert!(sb.is_sealed());
        for late in [
            Durable::Snapshot(Measure::of(99)),
            Durable::Note(Note {
                level: Level::Warn,
                glyph: "⚠",
                text: "late".into(),
            }),
            Durable::Finished(Outcome::Failed),
        ] {
            assert_eq!(
                sb.offer(&late),
                None,
                "nothing may commit after the end: {late:?}"
            );
        }
        // …and a frame after the end is doubly impossible.
        sb.frame(TASK, 9, &Frame::new("late spin"));
        assert_eq!(sb.committed().len(), 1, "exactly the one ending");
    }

    /// The acceptance the issue names: drive the sink through many frames and
    /// assert what commits is the **monotone subsequence plus terminal
    /// state** — not the frames.
    #[test]
    fn the_committed_story_is_the_monotone_subsequence_plus_terminal_state() {
        let mut sb = Scrollback::new();
        let mut at = 0u64;
        sb.record(
            TASK,
            at,
            &Durable::Started {
                label: "compacting".into(),
            },
        );

        // A realistic stream: lots of animation, occasional measurement, and
        // measurements that stall or stutter backwards as real counters do.
        for done in [1u64, 1, 2, 2, 2, 5, 4, 5, 9] {
            for _ in 0..7 {
                at += 1;
                sb.frame(TASK, at, &Frame::new(format!("t{at}")));
            }
            at += 1;
            sb.record(TASK, at, &Durable::Snapshot(Measure::of(done)));
        }
        at += 1;
        sb.record(TASK, at, &Durable::Finished(Outcome::Completed));

        assert_eq!(
            sb.committed(),
            &[
                Commit::Started {
                    label: "compacting".into()
                },
                Commit::Advanced(Measure::of(1)),
                Commit::Advanced(Measure::of(2)),
                Commit::Advanced(Measure::of(5)),
                Commit::Advanced(Measure::of(9)),
                Commit::Finished(Outcome::Completed),
            ],
            "committed scrollback must be the strictly-increasing subsequence \
             (1,2,5,9 — not the repeats, not the 4) bracketed by lifecycle"
        );
    }

    /// **The boundary, held against a LATER edit.**
    ///
    /// The two tests above prove a frame cannot reach scrollback *today*, and
    /// the type is why: there is no `Frame -> Commit` anywhere. But that is a
    /// property of what is absent, and absence is what a later well-meaning
    /// edit adds back — a `From` impl on the frame type "to reuse the label",
    /// a helper that turns the last frame into a final line. Either would make
    /// the defect representable again, and the monotone test would keep
    /// passing until someone actually called it.
    ///
    /// So this forbids the conversion itself. Sources are embedded at COMPILE
    /// time (no filesystem I/O in the unit tier) and the needles are built
    /// with `concat!` so this test's own source cannot match them.
    ///
    /// Note for whoever edits this next: the needles match **prose too**. Spell
    /// a forbidden signature out in a comment here and the guard fires on your
    /// documentation. That is the guard working, not a false positive — say it
    /// in words instead.
    #[test]
    fn nothing_may_add_a_conversion_from_a_frame_to_a_commit() {
        let forbidden = [
            concat!("From<", "Frame>"),
            concat!("From<&", "Frame>"),
            concat!("Frame)", " -> Commit"),
            concat!("Frame)", " -> Durable"),
            concat!("Frame)", " -> Option<Commit>"),
        ];
        for (name, src) in [
            ("progress.rs", include_str!("progress.rs")),
            ("tty/progress_sink.rs", include_str!("tty/progress_sink.rs")),
        ] {
            for needle in forbidden {
                assert!(
                    !src.contains(needle),
                    "{name} declares `{needle}` — that reopens the scrollback \
                     boundary this module exists to close. A frame is view \
                     state; if a caller needs a durable line, it must build a \
                     `Durable` and say what the line MEANS (#1864)."
                );
            }
        }
    }

    /// A note commits, carrying the ONE hue table rather than a second one.
    #[test]
    fn a_note_commits_and_speaks_the_existing_level_vocabulary() {
        let mut sb = Scrollback::new();
        let note = Note {
            level: Level::Warn,
            glyph: "⚠",
            text: "context trimmed".into(),
        };
        assert_eq!(
            sb_offer(&mut sb, Durable::Note(note.clone())),
            Some(Commit::Note(note))
        );
    }

    /// **The unbounded-buffer answer.** A high-rate producer must not grow
    /// memory in either shipped sink: `Scrollback` is bounded by the monotone
    /// story, `LatestFrame` holds exactly one frame.
    #[test]
    fn a_high_rate_producer_grows_neither_sink() {
        let mut sb = Scrollback::new();
        let mut latest = LatestFrame::new();
        for t in 0..10_000u64 {
            sb.frame(TASK, t, &Frame::new(format!("f{t}")));
            latest.frame(TASK, t, &Frame::new(format!("f{t}")));
            // A snapshot that never advances — the pathological producer.
            sb.record(TASK, t, &Durable::Snapshot(Measure::of(1)));
        }
        assert_eq!(
            sb.committed().len(),
            1,
            "10k frames + 10k non-advancing snapshots must commit exactly the \
             one genuine advance, not a backlog"
        );
        let (_, at, frame) = latest.peek().expect("a frame was published");
        assert_eq!(
            (*at, frame.label.as_str()),
            (9_999, "f9999"),
            "the coalescing sink keeps the NEWEST frame, not the oldest"
        );
    }

    /// **Dual-publish by reuse.** The four turn facts that already have a
    /// vocabulary map onto durable progress; nothing else does.
    #[test]
    fn the_lifecycle_seam_maps_onto_durable_progress() {
        use crate::lifecycle::LifecycleEvent as L;
        assert_eq!(
            from_lifecycle(&L::TurnStarted),
            Some(Durable::Started {
                label: "turn".into()
            })
        );
        assert_eq!(
            from_lifecycle(&L::TurnCompleted),
            Some(Durable::Finished(Outcome::Completed))
        );
        assert_eq!(
            from_lifecycle(&L::TurnFailed { reason: None }),
            Some(Durable::Finished(Outcome::Failed))
        );
        assert_eq!(
            from_lifecycle(&L::TurnCancelled),
            Some(Durable::Finished(Outcome::Cancelled))
        );
    }

    /// The half that matters more: a high-rate or transient lifecycle state is
    /// **not** durable. Committing a scrollback line every time the agent
    /// starts thinking, or every tool call, is the noise this contract exists
    /// to prevent — those are frame material, and this adapter refuses to
    /// decide otherwise on a caller's behalf.
    #[test]
    fn transient_lifecycle_states_are_not_durable_progress() {
        use crate::lifecycle::LifecycleEvent as L;
        for e in [
            L::Thinking,
            L::ToolActivity {
                tool: "run_command".into(),
            },
            L::Waiting,
            L::Blocked,
            L::Unblocked,
            L::SessionStarted {
                session_id: "s".into(),
            },
            L::SessionEnded,
        ] {
            assert_eq!(from_lifecycle(&e), None, "{e:?} must not commit");
        }
    }

    /// End to end through the adapter: a real lifecycle stream, replayed into
    /// a sink, commits the lifecycle story and nothing else — no line per
    /// `Thinking`, no line per tool call.
    #[test]
    fn a_replayed_lifecycle_stream_commits_only_the_turn_story() {
        use crate::lifecycle::LifecycleEvent as L;
        let mut sb = Scrollback::new();
        let stream = [
            L::SessionStarted {
                session_id: "s".into(),
            },
            L::TurnStarted,
            L::Thinking,
            L::ToolActivity {
                tool: "read_file".into(),
            },
            L::Thinking,
            L::ToolActivity {
                tool: "edit_file".into(),
            },
            L::Blocked,
            L::Unblocked,
            L::Thinking,
            L::TurnCompleted,
            L::Waiting,
        ];
        for (i, e) in stream.iter().enumerate() {
            if let Some(d) = from_lifecycle(e) {
                sb.record(TASK, i as u64, &d);
            }
        }
        assert_eq!(
            sb.committed(),
            &[
                Commit::Started {
                    label: "turn".into()
                },
                Commit::Finished(Outcome::Completed),
            ],
            "eleven lifecycle events, two committed lines"
        );
    }

    /// `NullSink` accepts everything and keeps nothing, so a producer needs no
    /// `Option` dance.
    #[test]
    fn the_null_sink_swallows_every_event() {
        let mut sink = NullSink;
        for t in 0..10 {
            sink.frame(TASK, t, &Frame::new("x"));
        }
        sink.record(TASK, 10, &Durable::Finished(Outcome::Cancelled));
    }

    /// The trait is object-safe: producers hold `&mut dyn ProgressSink`.
    #[test]
    fn the_sink_is_object_safe() {
        let mut sb = Scrollback::new();
        let dynamic: &mut dyn ProgressSink = &mut sb;
        dynamic.frame(TASK, 0, &Frame::new("x"));
        dynamic.record(TASK, 1, &Durable::Finished(Outcome::Completed));
        assert_eq!(sb.committed().len(), 1);
    }
}
