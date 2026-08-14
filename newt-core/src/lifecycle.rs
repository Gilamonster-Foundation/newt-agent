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
//! - **An observer cannot break the agent.** Observer calls run under
//!   [`std::panic::catch_unwind`]; a panicking integration loses its own
//!   event, not the session.
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

type Observer = Arc<dyn Fn(&LifecycleEvent) + Send + Sync>;

/// Registered observers, keyed by subscription id.
static OBSERVERS: RwLock<Vec<(u64, Observer)>> = RwLock::new(Vec::new());
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
        guard.retain(|(id, _)| *id != self.id);
        COUNT.store(guard.len(), Ordering::Release);
    }
}

/// Subscribe to lifecycle events.
///
/// The observer runs **on the emitting thread** — the agent's thread. It must
/// therefore be bounded and non-blocking: enqueue, or update a cell, and
/// return. Anything slower belongs on the observer's own thread.
pub fn subscribe(observer: impl Fn(&LifecycleEvent) + Send + Sync + 'static) -> Subscription {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = OBSERVERS.write().unwrap_or_else(PoisonError::into_inner);
    guard.push((id, Arc::new(observer)));
    COUNT.store(guard.len(), Ordering::Release);
    Subscription { id }
}

/// Is anyone listening? Call sites that would have to allocate to build an
/// event check this first.
#[must_use]
pub fn observed() -> bool {
    COUNT.load(Ordering::Acquire) > 0
}

/// Announce an event. Returns as soon as every observer has been handed it.
pub fn emit(event: LifecycleEvent) {
    if !observed() {
        return;
    }
    // Clone the (cheap, refcounted) observer list and release the lock before
    // calling out: an observer that re-enters `subscribe`/`emit` must not
    // deadlock against the registry.
    let observers: Vec<Observer> = {
        let guard = OBSERVERS.read().unwrap_or_else(PoisonError::into_inner);
        guard.iter().map(|(_, o)| Arc::clone(o)).collect()
    };
    for observer in observers {
        // An integration's bug is the integration's problem.
        let _ = catch_unwind(AssertUnwindSafe(|| observer(&event)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A collector that records only events emitted by the calling thread.
    /// The registry is process-global, so sibling tests (and the tty arbiter's
    /// own tests) emit concurrently; filtering by thread makes each test's
    /// expectations exact without serializing the suite.
    fn collector() -> (Subscription, Arc<Mutex<Vec<LifecycleEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mine = std::thread::current().id();
        let sub = subscribe(move |e| {
            if std::thread::current().id() == mine {
                sink.lock().unwrap().push(e.clone());
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
            .any(|(entry, _)| *entry == id));
        drop(sub);
        assert!(
            !OBSERVERS
                .read()
                .unwrap()
                .iter()
                .any(|(entry, _)| *entry == id),
            "drop must unregister exactly this observer"
        );
    }

    #[test]
    fn a_panicking_observer_cannot_break_the_agent() {
        let (sub, seen) = collector();
        let boom = subscribe(|_| panic!("integration bug"));
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // keep the test output clean
        emit(LifecycleEvent::Waiting);
        std::panic::set_hook(hook);
        drop(boom);
        drop(sub);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![LifecycleEvent::Waiting],
            "the surviving observer still receives the event"
        );
    }
}
