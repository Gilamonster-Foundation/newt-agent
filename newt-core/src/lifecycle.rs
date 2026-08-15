//! Generic agent lifecycle events — the integration seam.
//!
//! The agent announces *what it is doing* in its own vocabulary. It knows
//! nothing about who is listening or what they do with it: no terminal
//! manager, no cockpit, no wire protocol appears here or anywhere else in
//! `newt-core`. Integrations live in the UI tier (see `newt-tui::herdr`),
//! subscribe here, and translate.
//!
//! Three properties are load-bearing:
//!
//! - **Emission is not delivery.** [`emit`] hands the event to whatever
//!   observers exist and returns. An observer that wants to do I/O must
//!   enqueue and return; that contract is the whole reason this seam is
//!   allowed on the agent's hot path.
//! - **Nothing is required.** With no observers, [`emit`] is an atomic load
//!   and a branch, so a build with no integration behaves exactly like one
//!   without this module. [`observed`] lets a call site skip even the cost of
//!   *building* an event.
//! - **Observers are TRUSTED SYNCHRONOUS CALLBACKS, and only panics are
//!   isolated.** Observer calls run under [`std::panic::catch_unwind`], so a
//!   panicking integration loses its own event rather than the session. That
//!   is the whole of the guarantee: an observer that *blocks* — a lock it
//!   waits on, an I/O call, a sleep — blocks the agent's hot path, because
//!   [`emit`] calls it inline on the emitting thread. This seam does not spawn,
//!   time out, or queue on an observer's behalf. An integration that needs to
//!   do work must enqueue and return (see `newt-tui::herdr`, which folds into
//!   shared state and nudges a worker thread). Earlier wording here claimed an
//!   observer "cannot break the agent", which overstated it: a blocking
//!   observer can, and no mechanism here prevents that.
//!
//! The event vocabulary is deliberately semantic. Every variant corresponds to
//! a place in the agent where that thing demonstrably happens — an accepted
//! turn, a dispatched inference request, a tool call about to run, a prompt
//! window that owns stdin. None of it is inferred by inspecting rendered
//! prompt text, because rendered text is a presentation detail that changes
//! for cosmetic reasons and would make integrations lie.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

/// Something the agent did, in the agent's own terms.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// A chat session began (or was re-anchored by `/new`), with the id the
    /// agent uses for it.
    SessionStarted { session_id: String },
    /// The agent is at its prompt, waiting for the operator. Not "blocked" —
    /// nothing is pending; the human simply has the floor.
    Waiting,
    /// A non-empty turn was accepted and is running.
    TurnStarted,
    /// An inference request is in flight.
    Thinking,
    /// A tool call is about to execute.
    ToolActivity { tool: String },
    /// The process is blocked on a human decision: a prompt window is open
    /// and owns stdin (permission gate, question, modal).
    Blocked,
    /// That prompt window closed; the previous state resumes.
    Unblocked,
    /// The turn finished normally.
    TurnCompleted,
    /// The turn ended in an error.
    TurnFailed { reason: Option<String> },
    /// The turn was interrupted by the operator.
    TurnCancelled,
    /// The session is ending.
    SessionEnded,
}

/// An event together with the session it belongs to.
///
/// Session identity rides EVERY event, not just [`LifecycleEvent::SessionStarted`]
/// — an integration that watches one session must be able to reject another
/// session's `Thinking` without having tracked a start it may never have seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEnvelope {
    /// The session this event belongs to, when one is established.
    ///
    /// `None` only before any session has been announced (early startup) or
    /// after [`clear_active_session`]. It is not a wildcard: a filtered
    /// subscription does not receive `None`-scoped events.
    pub session_id: Option<String>,
    pub event: LifecycleEvent,
}

type Observer = Arc<dyn Fn(&LifecycleEnvelope) + Send + Sync>;

/// Registered observers, keyed by subscription id, each with the session it
/// wants (`None` = every session).
static OBSERVERS: RwLock<Vec<(u64, Option<String>, Observer)>> = RwLock::new(Vec::new());

/// The session that currently OWNS the agent — the one whose turn is running
/// and whose prompt owns the terminal.
///
/// Why an ambient cell rather than a handle threaded to every call site: two
/// emitters live in process-global infrastructure that has no session and
/// cannot be given one without changes outside this seam —
/// `tty::arbiter::notify_prompt_observer` (the line arbiter is a singleton by
/// construction: one terminal, one writer) and `agentic::announce_tool_activity`
/// (deep in tool dispatch). For those two, attribution to the owning session is
/// not a guess: a `PromptWindow` blocks whichever session holds the terminal,
/// and a tool call runs inside whichever session's turn is executing, and the
/// REPL runs turns synchronously so exactly one session can be in either state.
///
/// What would invalidate it: genuinely parallel turns inside ONE process. Tabs
/// (#1669) do not — the ADR keeps one active tab with synchronous turns — but a
/// future that runs two turns concurrently in-process must replace this cell
/// with a real per-session context, and the call sites above are the two that
/// would need plumbing. Separate processes are unaffected: each has its own.
static ACTIVE_SESSION: RwLock<Option<String>> = RwLock::new(None);
/// Mirror of `OBSERVERS.len()`, so the (overwhelmingly common) unobserved
/// case costs one relaxed load and never touches the lock.
static COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A live subscription. Dropping it unsubscribes — so an integration that
/// goes away stops being called, and tests cannot leak observers into each
/// other.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut guard = OBSERVERS.write().unwrap_or_else(PoisonError::into_inner);
        guard.retain(|(id, _, _)| *id != self.id);
        COUNT.store(guard.len(), Ordering::Release);
    }
}

/// Subscribe to lifecycle events.
///
/// The observer runs **on the emitting thread** — the agent's thread. It must
/// therefore be bounded and non-blocking: enqueue, or update a cell, and
/// return. Anything slower belongs on the observer's own thread.
pub fn subscribe(observer: impl Fn(&LifecycleEnvelope) + Send + Sync + 'static) -> Subscription {
    register(None, Arc::new(observer))
}

/// Subscribe to ONE session's events. Events from any other session — and
/// unscoped (`None`) events — are not delivered.
///
/// This is what keeps concurrent integrations honest: session B's `Waiting`
/// cannot move session A's state machine, and A's `SessionEnded` cannot shut
/// B's reporting down, because B's observer never sees them.
pub fn subscribe_session(
    session_id: impl Into<String>,
    observer: impl Fn(&LifecycleEnvelope) + Send + Sync + 'static,
) -> Subscription {
    register(Some(session_id.into()), Arc::new(observer))
}

fn register(want: Option<String>, observer: Observer) -> Subscription {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = OBSERVERS.write().unwrap_or_else(PoisonError::into_inner);
    guard.push((id, want, observer));
    COUNT.store(guard.len(), Ordering::Release);
    Subscription { id }
}

/// A fresh, collision-free identity for one Newt SESSION.
///
/// # What a lifecycle `session_id` means
///
/// It identifies **one running Newt** — one process, one tab, one pane — and is
/// stable for that session's entire lifetime. It is deliberately NOT the
/// conversation id:
///
/// | | changes when | scope |
/// |---|---|---|
/// | **session id** (this) | never, after startup | the running Newt / its Herdr pane |
/// | conversation id ([`crate::new_conversation_id`]) | `/new`, `/resume`, `/conversation restore`, roadmap navigation, persona rotation | one thread of conversation *within* a session |
///
/// Mixing them is what #1662 was reopened to fix. `run_chat` stamped startup
/// events with the session id and `/new` re-stamped ownership with the
/// CONVERSATION id, so one field carried two different kinds of identity and an
/// observer could not tell which it had. A Herdr pane that had adopted a
/// session then saw later events bearing a conversation id it did not
/// recognize — a name for the same agent that simply failed to match.
///
/// Conversation switching is therefore NOT a lifecycle ownership event: the
/// owner is set once, at startup, and never becomes stale because it never
/// changes. If a future feature needs Herdr to track the active conversation,
/// that is a distinct field carrying a distinct id, not a second meaning
/// overloaded onto this one.
///
/// Same shape as [`crate::new_conversation_id`] — nanosecond clock plus a v4
/// UUID — deliberately reusing that proven generator rather than standing a
/// second scheme beside it. The previous session id was
/// `SystemTime::now().as_secs()`, so two Newts launched in the same second (a
/// script, a tab-restore, a Herdr layout opening several panes) shared one
/// lifecycle identity and each would answer to the other's events.
#[must_use]
pub fn new_session_id() -> SessionId {
    SessionId(format!("session-{}", crate::new_conversation_id()))
}

/// The identity of one running Newt session — see [`new_session_id`].
///
/// A newtype rather than a `String`, deliberately. The bug this reopened #1662
/// was a *conversation* id being handed to [`set_active_session`], and that is
/// exactly the sort of mistake the repo's "make the bug unrepresentable" rule
/// exists for: there is no public way to build a `SessionId` from an arbitrary
/// string, so `set_active_session(active_conversation_id.clone())` no longer
/// compiles. The two identity domains cannot be confused by accident again —
/// only by someone deliberately reaching for the test-only constructor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// Borrow the wire form (what rides in [`LifecycleEnvelope::session_id`]).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild a `SessionId` from a string that is ALREADY a session id.
    ///
    /// For tests and for any future path that legitimately restores a session
    /// identity it previously issued (a resumed pane, a supervisor handing an
    /// id down). Never call this with a conversation id — that reintroduces
    /// precisely the confusion the newtype prevents.
    #[must_use]
    pub fn from_issued(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Declare which session owns the agent from now on.
///
/// Called ONCE, when the session starts. A conversation switch is not an
/// ownership change — see [`new_session_id`] for why the two identities are
/// kept apart, and [`ACTIVE_SESSION`] for why the two infrastructure emitters
/// need an ambient owner at all.
pub fn set_active_session(session_id: &SessionId) {
    *ACTIVE_SESSION
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Some(session_id.as_str().to_string());
}

/// Forget the owning session (the session ended and nothing replaced it).
pub fn clear_active_session() {
    *ACTIVE_SESSION
        .write()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

/// The session that currently owns the agent, if any.
#[must_use]
pub fn active_session() -> Option<String> {
    ACTIVE_SESSION
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Is anyone listening? Call sites that would have to allocate to build an
/// event check this first.
#[must_use]
pub fn observed() -> bool {
    COUNT.load(Ordering::Acquire) > 0
}

/// Announce an event, attributed to the session that currently owns the agent.
/// Returns as soon as every matching observer has been handed it.
pub fn emit(event: LifecycleEvent) {
    if !observed() {
        return;
    }
    emit_for(active_session(), event);
}

/// Announce an event attributed to an EXPLICIT session — for call sites that
/// hold the id and should not depend on the ambient owner. `SessionStarted`
/// uses this, so a start is always self-describing even if the ownership cell
/// has not been updated yet.
pub fn emit_for(session_id: Option<String>, event: LifecycleEvent) {
    if !observed() {
        return;
    }
    let envelope = LifecycleEnvelope { session_id, event };
    // Clone the (cheap, refcounted) matching observers and release the lock
    // before calling out: an observer that re-enters `subscribe`/`emit` must
    // not deadlock against the registry.
    let observers: Vec<Observer> = {
        let guard = OBSERVERS.read().unwrap_or_else(PoisonError::into_inner);
        guard
            .iter()
            .filter(|(_, want, _)| match want {
                // Unfiltered subscription: everything, including unscoped.
                None => true,
                // Filtered: this session only. An unscoped event is NOT a
                // wildcard — it belongs to no session, so it matches none.
                Some(want) => envelope.session_id.as_deref() == Some(want.as_str()),
            })
            .map(|(_, _, o)| Arc::clone(o))
            .collect()
    };
    for observer in observers {
        // A panicking integration loses its own event. A BLOCKING one blocks
        // the agent — see the module docs; that is not isolated here.
        let _ = catch_unwind(AssertUnwindSafe(|| observer(&envelope)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A collector that records only envelopes emitted by the calling thread.
    /// The registry is process-global, so sibling tests (and the tty arbiter's
    /// own tests) emit concurrently; filtering by thread makes each test's
    /// expectations exact without serializing the suite.
    fn collector() -> (Subscription, Arc<Mutex<Vec<LifecycleEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mine = std::thread::current().id();
        let sub = subscribe(move |e| {
            if std::thread::current().id() == mine {
                sink.lock().unwrap().push(e.event.clone());
            }
        });
        (sub, seen)
    }

    /// The same, scoped to ONE session id.
    fn session_collector(id: &str) -> (Subscription, Arc<Mutex<Vec<LifecycleEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mine = std::thread::current().id();
        let sub = subscribe_session(id, move |e| {
            if std::thread::current().id() == mine {
                sink.lock().unwrap().push(e.event.clone());
            }
        });
        (sub, seen)
    }

    #[test]
    fn events_reach_subscribers_and_stop_at_unsubscribe() {
        let (sub, seen) = collector();
        emit(LifecycleEvent::TurnStarted);
        emit(LifecycleEvent::ToolActivity {
            tool: "read_file".into(),
        });
        drop(sub);
        emit(LifecycleEvent::TurnCompleted);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                LifecycleEvent::TurnStarted,
                LifecycleEvent::ToolActivity {
                    tool: "read_file".into()
                }
            ],
            "a dropped subscription must stop receiving"
        );
    }

    // `observed()` is the fast-path gate call sites use to skip building an
    // event; a live subscription must make it true, and dropping one must
    // remove exactly that entry from the registry (checked by id so a
    // concurrently-running sibling test cannot perturb the assertion).
    #[test]
    fn subscription_registration_is_exact() {
        let sub = subscribe(|_| {});
        let id = sub.id;
        assert!(observed());
        assert!(OBSERVERS
            .read()
            .unwrap()
            .iter()
            .any(|(entry, _, _)| *entry == id));
        drop(sub);
        assert!(
            !OBSERVERS
                .read()
                .unwrap()
                .iter()
                .any(|(entry, _, _)| *entry == id),
            "drop must unregister exactly this observer"
        );
    }

    #[test]
    fn every_event_carries_its_session_not_just_the_start() {
        // The defect this closes: identity used to ride SessionStarted alone,
        // so an integration that attached mid-session could not attribute a
        // single later event.
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mine = std::thread::current().id();
        let sub = subscribe(move |e| {
            if std::thread::current().id() == mine {
                sink.lock().unwrap().push(e.session_id.clone());
            }
        });
        emit_for(Some("sess-A".into()), LifecycleEvent::Thinking);
        emit_for(Some("sess-A".into()), LifecycleEvent::TurnCompleted);
        drop(sub);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some("sess-A".to_string()), Some("sess-A".to_string())],
            "identity rides every event"
        );
    }

    // ── adversarial concurrent sessions (#1662) ───────────────────────────
    //
    // Two integrations, one process, one registry. Each must be deaf to the
    // other's session. These drive the REAL subscribe/emit seam rather than a
    // hand-rolled filter, so a regression in the matcher fails them.

    #[test]
    fn a_thinking_does_not_reach_b() {
        let (sub_a, a) = session_collector("A");
        let (sub_b, b) = session_collector("B");
        emit_for(Some("A".into()), LifecycleEvent::Thinking);
        drop(sub_a);
        drop(sub_b);
        assert_eq!(*a.lock().unwrap(), vec![LifecycleEvent::Thinking]);
        assert!(
            b.lock().unwrap().is_empty(),
            "B must not observe A's Thinking"
        );
    }

    #[test]
    fn b_waiting_does_not_reach_a() {
        let (sub_a, a) = session_collector("A");
        let (sub_b, b) = session_collector("B");
        emit_for(Some("B".into()), LifecycleEvent::Waiting);
        drop(sub_a);
        drop(sub_b);
        assert_eq!(*b.lock().unwrap(), vec![LifecycleEvent::Waiting]);
        assert!(
            a.lock().unwrap().is_empty(),
            "A must not observe B's Waiting — B going idle cannot idle A"
        );
    }

    #[test]
    fn a_session_ended_does_not_end_b_and_b_keeps_reporting() {
        let (sub_a, a) = session_collector("A");
        let (sub_b, b) = session_collector("B");
        emit_for(Some("A".into()), LifecycleEvent::SessionEnded);
        // B must still be live afterwards — the shutdown was not contagious.
        emit_for(Some("B".into()), LifecycleEvent::TurnStarted);
        emit_for(Some("B".into()), LifecycleEvent::TurnCompleted);
        drop(sub_a);
        drop(sub_b);
        assert_eq!(*a.lock().unwrap(), vec![LifecycleEvent::SessionEnded]);
        assert_eq!(
            *b.lock().unwrap(),
            vec![LifecycleEvent::TurnStarted, LifecycleEvent::TurnCompleted],
            "B continues reporting after A ended"
        );
    }

    #[test]
    fn an_unscoped_event_is_not_a_wildcard() {
        // `None` means "belongs to no session", not "belongs to all of them".
        // Treating it as a wildcard would hand every filtered integration the
        // early-startup events of a session that is not theirs.
        let (sub_a, a) = session_collector("A");
        let (sub_all, all) = collector();
        emit_for(None, LifecycleEvent::Waiting);
        drop(sub_a);
        drop(sub_all);
        assert!(a.lock().unwrap().is_empty(), "filtered gets nothing");
        assert_eq!(
            *all.lock().unwrap(),
            vec![LifecycleEvent::Waiting],
            "unfiltered still gets it"
        );
    }

    #[test]
    fn the_active_session_stamps_the_infrastructure_emitters() {
        // `emit` (no explicit id) is what tty::arbiter and tool dispatch call.
        // It must attribute to whoever owns the agent.
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        let (sub, seen) = session_collector("owner");
        let owner = SessionId::from_issued("owner");
        set_active_session(&owner);
        emit(LifecycleEvent::Blocked);
        clear_active_session();
        emit(LifecycleEvent::Unblocked); // now unscoped — must not arrive
        drop(sub);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![LifecycleEvent::Blocked],
            "scoped while owned; unscoped after clearing"
        );
    }

    /// #1662: two sessions created back to back must not share an identity.
    ///
    /// The old id was `SystemTime::now().as_secs()`, so any two Newts launched
    /// inside the same second — a script, a shell tab restore, a Herdr layout
    /// opening several panes at once — were literally the same session as far
    /// as this seam was concerned, and each would answer to the other's events.
    /// A loop is the honest test: it fails on the old scheme (all identical)
    /// and passes on nanos + UUID.
    #[test]
    fn concurrently_created_sessions_have_distinct_identities() {
        let ids: std::collections::BTreeSet<String> =
            (0..64).map(|_| new_session_id().to_string()).collect();
        assert_eq!(ids.len(), 64, "every session identity must be unique");

        // And across real threads, which is the case that actually occurs.
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| new_session_id().to_string()))
            .collect();
        let threaded: std::collections::BTreeSet<String> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(threaded.len(), 8, "concurrent creation must not collide");
    }

    /// #1662: a conversation switch must not change who owns lifecycle events.
    ///
    /// The regression: `/new` used to call `set_active_session` with the new
    /// CONVERSATION id, so one field carried two kinds of identity. A pane that
    /// had adopted the session then saw later `Thinking` / tool events stamped
    /// with an id it did not recognize — the agent renamed itself mid-session.
    ///
    /// The newtype makes the original mistake uncompilable, so what this pins
    /// is the SEMANTIC half: ownership survives any number of conversation
    /// switches, and events after them still reach the session's observer.
    #[test]
    fn a_conversation_switch_cannot_misattribute_later_events() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        let session = new_session_id();
        let (sub, seen) = session_collector(session.as_str());
        set_active_session(&session);

        // Startup announces the session.
        emit_for(
            Some(session.to_string()),
            LifecycleEvent::SessionStarted {
                session_id: session.to_string(),
            },
        );
        emit(LifecycleEvent::Thinking);

        // Now switch conversations several times. `/new`, `/resume`,
        // `/conversation restore`, roadmap navigation and persona rotation all
        // mint or adopt a conversation id; NONE of them is a lifecycle
        // ownership event, so none of them touches the cell.
        for _ in 0..3 {
            let _conversation = crate::new_conversation_id();
        }

        // Events after the switches still belong to the same session.
        emit(LifecycleEvent::Thinking);
        emit(LifecycleEvent::ToolActivity {
            tool: "run_command".to_string(),
        });
        emit(LifecycleEvent::TurnCompleted);
        drop(sub);

        let got = seen.lock().unwrap().clone();
        assert!(
            got.contains(&LifecycleEvent::TurnCompleted),
            "events after a conversation switch must still reach the session: {got:?}"
        );
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, LifecycleEvent::Thinking))
                .count(),
            2,
            "both Thinking events belong to this session: {got:?}"
        );
        assert_eq!(
            active_session().as_deref(),
            Some(session.as_str()),
            "a conversation switch must not re-anchor lifecycle ownership"
        );
    }

    #[test]
    #[serial_test::serial(panic_hook)]
    fn a_panicking_observer_cannot_break_the_agent() {
        // Serialized on `panic_hook`: this swaps the PROCESS-GLOBAL panic hook
        // to keep test output clean, and doing that while a sibling test is
        // panicking (or asserting) would silence or corrupt its report. The
        // lane is the smallest correct fix — the alternative, leaving the hook
        // alone, floods the suite output with an intentional panic.
        let (sub, seen) = collector();
        let boom = subscribe(|_| panic!("integration bug"));
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        emit(LifecycleEvent::Waiting);
        std::panic::set_hook(hook);
        drop(boom);
        drop(sub);
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&LifecycleEvent::Waiting),
            "the surviving observer still receives the event"
        );
    }
}
