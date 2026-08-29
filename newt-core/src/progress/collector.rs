//! **The live producer** D2a dual-publishes from (#1864).
//!
//! [`from_lifecycle`](super::from_lifecycle) can map an event, but a mapping
//! nobody feeds is a contract that has never met real traffic — it compiles,
//! its tests pass against its own fixtures, and the first integration is still
//! ahead of it. That is the shape that looks finished and is not. So this
//! subscribes to the lifecycle seam the agent already emits on, and the
//! progress contract sees every real turn.
//!
//! # Collect-only, and silent BY CONSTRUCTION
//!
//! Dual-publish means the same event reaches both paths while the old one stays
//! authoritative. The new path is therefore allowed to see everything and
//! permitted to show nothing: this collector **renders nothing, emits no bytes,
//! and owns no file descriptor**.
//!
//! That is not a promise about behaviour, it is a property of the type.
//! [`LifecycleCollector`] holds a [`Scrollback`] and a counter — no stream
//! selector, no line capability, no writer, no file descriptor, and no
//! reference to anything that has one. There is no field through which a byte
//! could leave, and `the_collector_owns_no_writer` fails the build if one
//! appears.
//!
//! (That guard scans this file, so it matches PROSE as well as code. Naming a
//! forbidden type here in backticks trips it — which is the guard working. Say
//! it in words, as above.)
//!
//! **Why construction rather than reachability**: #1866 is this hazard one
//! layer down. `PromptWindow::ask` never consults `protocol_mode()`, and the
//! zero-byte veto holds for prompts only because no protocol-mode entry point
//! currently reaches a `PromptWindow`. That invariant is true by accident of
//! the call graph, and the next entry point breaks it silently with nothing to
//! catch it. A collector whose silence rested on "nothing calls its render
//! path" would be the same latent defect, freshly minted.
//!
//! # Bounded, for the reason this module already argued
//!
//! [`super`]'s docs reject unbounded buffering between a high-rate producer and
//! a slow consumer. A collector that accumulated every turn of a long session
//! would be that leak, written by the module that warned about it. So the
//! scrollback is **per turn**: [`Durable::Started`] replaces it, and what
//! persists across a session is a `u64` count. One turn's monotone story is the
//! high-water mark, whatever the session's length.

use std::sync::{Arc, Mutex, PoisonError};

use super::{Durable, ProgressSink, Scrollback, TaskId};
use crate::lifecycle::{subscribe, subscribe_session, LifecycleEnvelope, Subscription};

/// The task id turn-derived progress carries.
pub const TURN_TASK: TaskId = "turn";

/// What the collector holds. Deliberately: a scrollback and a number.
#[derive(Debug, Default)]
struct State {
    /// This turn's committed story. Replaced when a new turn starts, so a long
    /// session does not accumulate.
    current: Scrollback,
    /// Turns whose terminal state has been recorded.
    completed_turns: u64,
    /// Durable events recorded, ever. A counter, not a log.
    recorded: u64,
}

/// A collect-only observer of the lifecycle seam.
///
/// Cheap to clone (`Arc` inside) so the subscription closure and the inspecting
/// caller share one state.
#[derive(Debug, Clone, Default)]
pub struct LifecycleCollector {
    state: Arc<Mutex<State>>,
}

impl LifecycleCollector {
    /// A fresh collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one lifecycle envelope. Non-blocking and allocation-light: this
    /// runs on the agent's own thread, per the lifecycle seam's contract.
    pub fn observe(&self, envelope: &LifecycleEnvelope) {
        let Some(durable) = super::from_lifecycle(&envelope.event) else {
            return;
        };
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // A new turn starts a new scrollback — the bound.
        if matches!(durable, Durable::Started { .. }) {
            st.current = Scrollback::new();
        }
        st.recorded += 1;
        st.current.record(TURN_TASK, 0, &durable);
        if st.current.is_sealed() {
            st.completed_turns += 1;
        }
    }

    /// The current turn's committed story.
    #[must_use]
    pub fn committed(&self) -> Vec<super::Commit> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .current
            .committed()
            .to_vec()
    }

    /// How many turns have reached terminal state.
    #[must_use]
    pub fn completed_turns(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .completed_turns
    }

    /// How many durable events have been recorded, ever.
    #[must_use]
    pub fn recorded(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recorded
    }
}

/// Dual-publish every session's turns into a collector.
///
/// Returns the collector and its [`Subscription`]; dropping the subscription
/// stops delivery, so a caller cannot leak an observer.
#[must_use]
pub fn collect_all() -> (LifecycleCollector, Subscription) {
    let collector = LifecycleCollector::new();
    let sub = {
        let c = collector.clone();
        subscribe(move |envelope| c.observe(envelope))
    };
    (collector, sub)
}

/// Dual-publish ONE session's turns into a collector.
///
/// Preferred wherever a session id exists — and required in tests, where the
/// observer registry is process-global and an unfiltered subscription would
/// see sibling tests' events. Unique ids keep concurrent tests out of each
/// other without serializing the suite, which is the pattern `lifecycle`'s own
/// tests already use.
#[must_use]
pub fn collect_session(session_id: impl Into<String>) -> (LifecycleCollector, Subscription) {
    let collector = LifecycleCollector::new();
    let sub = {
        let c = collector.clone();
        subscribe_session(session_id, move |envelope| c.observe(envelope))
    };
    (collector, sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{emit_for, LifecycleEvent as L};
    use crate::progress::{Commit, Outcome};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique session per test: the observer registry is process-global, so
    /// this is what keeps siblings out without a lock. (#1880 is what happens
    /// when a test in this crate depends on global quiet instead.)
    fn unique_session() -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        format!("d2a-collector-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    /// **The live producer, end to end.** Real lifecycle events, emitted the
    /// way the agent emits them, reach the progress contract.
    #[test]
    fn real_lifecycle_traffic_reaches_the_progress_contract() {
        let s = unique_session();
        let (c, _sub) = collect_session(s.clone());
        for e in [
            L::TurnStarted,
            L::Thinking,
            L::ToolActivity {
                tool: "read_file".into(),
            },
            L::Thinking,
            L::TurnCompleted,
        ] {
            emit_for(Some(s.clone()), e);
        }
        assert_eq!(
            c.committed(),
            vec![
                Commit::Started {
                    label: "turn".into()
                },
                Commit::Finished(Outcome::Completed),
            ],
            "five real events, two committed lines"
        );
        assert_eq!(c.completed_turns(), 1);
    }

    /// **Bounded across a long session.** A thousand turns leave one turn's
    /// story in memory, not a thousand — the leak this module's own docs
    /// argue against.
    #[test]
    fn a_long_session_does_not_accumulate() {
        let s = unique_session();
        let (c, _sub) = collect_session(s.clone());
        for _ in 0..1_000 {
            emit_for(Some(s.clone()), L::TurnStarted);
            emit_for(Some(s.clone()), L::Thinking);
            emit_for(Some(s.clone()), L::TurnCompleted);
        }
        assert_eq!(c.completed_turns(), 1_000, "every turn was seen");
        assert_eq!(c.recorded(), 2_000, "and every durable event recorded");
        assert_eq!(
            c.committed().len(),
            2,
            "but only the CURRENT turn's story is retained: {:?}",
            c.committed()
        );
    }

    /// **The silence, proven at the seam.** Installing the collector must not
    /// change what any other observer receives — same events, same order, same
    /// payloads. This is the "byte-identical operator output" property stated
    /// where it is actually checkable: the collector is downstream of nothing
    /// and perturbs nobody.
    #[test]
    fn installing_the_collector_does_not_change_what_other_observers_see() {
        fn drive(session: &str) -> Vec<L> {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let sub = {
                let seen = Arc::clone(&seen);
                subscribe_session(session.to_string(), move |env| {
                    seen.lock().unwrap().push(env.event.clone());
                })
            };
            for e in [
                L::TurnStarted,
                L::Thinking,
                L::ToolActivity {
                    tool: "edit_file".into(),
                },
                L::Blocked,
                L::Unblocked,
                L::TurnFailed {
                    reason: Some("boom".into()),
                },
            ] {
                emit_for(Some(session.to_string()), e);
            }
            drop(sub);
            let out = seen.lock().unwrap().clone();
            out
        }

        // Without the collector installed.
        let bare = drive(&unique_session());

        // With it installed on the same stream.
        let s = unique_session();
        let (c, _sub) = collect_session(s.clone());
        let dual = drive(&s);

        assert_eq!(
            bare, dual,
            "an existing observer must receive exactly the same stream whether \
             or not progress is dual-published"
        );
        assert_eq!(
            c.committed(),
            vec![
                Commit::Started {
                    label: "turn".into()
                },
                Commit::Finished(Outcome::Failed),
            ],
            "…and the collector still saw the turn"
        );
    }

    /// **Silence by construction, not by reachability.** The collector owns no
    /// writer, so there is no path by which it could emit a byte — the
    /// distinction #1866 exists to make, where a veto holds only because
    /// nothing currently calls the path that would violate it.
    ///
    /// Source is embedded at COMPILE time (no filesystem I/O in the unit tier);
    /// needles are built with `concat!` so this file cannot match itself.
    #[test]
    fn the_collector_owns_no_writer() {
        let src = include_str!("collector.rs");
        // Everything a byte could leave through.
        for needle in [
            concat!("print", "!("),
            concat!("eprint", "!("),
            concat!("write", "!("),
            concat!("writeln", "!("),
            concat!("io::", "stdout"),
            concat!("io::", "stderr"),
            concat!("tty::", "Sink"),
            concat!("Line", "Caps"),
            concat!("Terminal::", "emit_line"),
            concat!(".emit", "("),
        ] {
            assert!(
                !src.contains(needle),
                "the collector references `{needle}` — it must own no writer at \
                 all. Silence here is a property of the TYPE, not of which paths \
                 happen to be reachable (#1866, #1864 constraint 3)."
            );
        }
    }
}
