//! Flock-origin ingress: verified briefs and reviews addressed to this member
//! become durable queue rows, and the loop head turns one row at a time into a
//! prompt with its own `flock` origin (never `WebInjected`, which the TUI
//! records as the operator's).
//!
//! A signed brief is still untrusted content. The text is rendered inert, the
//! prompt runs under the same tool confinement as any other, and the TUI
//! treats it as non-operator input (no slash commands, no `!` escapes, no
//! history entry).

// The follower and the doorbell are composed by the launcher, which hands the
// same `Wake` to the surface and to `FlockIntake::start`; until that lands they
// are exercised by tests only.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A one-bit doorbell from the intake thread to an idle input surface: a
/// task was queued, so look at the queue now instead of waiting for a key.
#[derive(Clone, Default, Debug)]
pub(crate) struct Wake(Arc<AtomicBool>);

impl Wake {
    /// Ring it.
    pub(crate) fn fire(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether it rang since the last `take`.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

/// What the loop head found in the flock queue.
#[derive(Debug, Default)]
pub(crate) struct Next {
    /// The next verified task, if any.
    pub(crate) task: Option<newt_core::FlockTask>,
    /// Why each row that failed verification on the way was refused.
    pub(crate) rejected: Vec<String>,
}

/// Check a queued row against its own evidence. Only a build that can verify
/// polls at all (see [`next_flock_task`]).
#[cfg(feature = "flock")]
fn verify_row(row: &newt_core::FlockTask) -> Result<(), String> {
    newt_flock::verify(&to_new(&row.task)).map(drop)
}

#[cfg(feature = "flock")]
fn to_new(t: &newt_core::NewFlockTask) -> newt_flock::NewTask {
    newt_flock::NewTask {
        body: t.body.clone(),
        flock_cid: t.flock_cid.clone(),
        event_cid: t.event_cid.clone(),
        evidence_cid: t.evidence_cid.clone(),
        evidence: t.evidence.clone(),
    }
}

/// The next flock task that verifies, in queue order. A row that does not
/// verify is marked rejected (so it is never offered again) and the next is
/// tried; nothing here runs a task. A store error ends the poll quietly: the
/// queue is durable, so the next poll sees the same rows.
///
/// A build without `flock` cannot verify anything, and lack of a verifier is
/// not a failed verification: it leaves every row queued and untouched for a
/// build that can.
pub(crate) fn next_flock_task(store: &newt_core::ConversationStore, conversation: &str) -> Next {
    #[cfg(not(feature = "flock"))]
    {
        let _ = (store, conversation);
        Next::default()
    }
    #[cfg(feature = "flock")]
    {
        let mut next = Next::default();
        loop {
            let Ok(Some(row)) = store.peek_flock_task(conversation) else {
                return next;
            };
            match verify_row(&row) {
                Ok(()) => {
                    next.task = Some(row);
                    return next;
                }
                Err(why) => {
                    // if it cannot even be marked, stop rather than spin on it
                    if store.reject_flock_task(&row).is_err() {
                        return next;
                    }
                    next.rejected.push(why);
                }
            }
        }
    }
}

/// Decides, at the loop head, whether a queued flock task may be selected
/// instead of reading the operator. Operator input takes precedence: nothing
/// is selected while the active tab cannot accept work, and a task whose
/// admission just failed is not offered again until the operator has been
/// read once. The row itself stays queued either way.
#[derive(Debug, Default)]
pub(crate) struct FlockTurnGate {
    yield_once: bool,
}

impl FlockTurnGate {
    /// The next verified task, or nothing when the operator goes first.
    pub(crate) fn poll(
        &mut self,
        store: &newt_core::ConversationStore,
        conversation: &str,
        tab_degraded: bool,
    ) -> Next {
        if std::mem::take(&mut self.yield_once) || tab_degraded {
            return Next::default();
        }
        next_flock_task(store, conversation)
    }

    /// The task just offered was not accepted (refused by the tab or the
    /// store): read the operator before offering it again.
    pub(crate) fn yield_to_operator(&mut self) {
        self.yield_once = true;
    }
}

/// The text shown to the operator when a flock task starts a turn: what it is
/// and where it came from, from the verified evidence rather than the prompt.
pub(crate) fn provenance_line(row: &newt_core::FlockTask) -> String {
    let head = row.task.body.lines().nth(1).unwrap_or_default();
    format!("[flock] task from the flock log → {head} (runs now; confined like any other turn)")
}

#[cfg(feature = "flock")]
mod follow {
    use super::{to_new_task, Wake};
    use newt_flock::{Config, Error, Inbox, Intake, NewTask, Sink};

    /// Queues what the intake finds, and rings the doorbell for anything new.
    struct StoreSink {
        store: newt_core::ConversationStore,
        conversation: String,
        wake: Wake,
    }

    impl Sink for StoreSink {
        fn enqueue(&mut self, task: &NewTask) -> Result<bool, String> {
            let fresh = self
                .store
                .enqueue_flock_task(&self.conversation, &to_new_task(task))
                .map_err(|e| e.to_string())?;
            let new = fresh == newt_core::InjectOutcome::Enqueued;
            if new {
                self.wake.fire();
            }
            Ok(new)
        }
    }

    /// The running follower: dropping it stops and joins the worker.
    pub(crate) struct FlockIntake {
        _inbox: Inbox,
    }

    impl FlockIntake {
        /// Follow the flock as `member` and queue its open tasks for
        /// `conversation`, once each catch-up of the verified log is saved
        /// (never from a saved prefix on its own).
        ///
        /// # Errors
        /// The session or the saved state was refused.
        pub(crate) fn start(
            cfg: Config,
            member: &str,
            store: newt_core::ConversationStore,
            conversation: &str,
            wake: Wake,
        ) -> Result<Self, Error> {
            let mut intake = Intake::for_genesis(member, &cfg.genesis).map_err(Error::Session)?;
            let mut sink = StoreSink {
                store,
                conversation: conversation.to_owned(),
                wake,
            };
            let inbox = Inbox::follow(cfg, move |f| {
                if let Some(ui) = crate::flock_ui::installed() {
                    ui.observe(newt_flock::view(f.board()));
                }
                intake.reconcile(f, &mut sink);
            })?;
            Ok(Self { _inbox: inbox })
        }
    }
}

// Composed by the launcher (which passes the surface and this the same
// doorbell); not wired in 12b.
#[cfg(feature = "flock")]
#[allow(unused_imports)] // the launcher imports it
pub(crate) use follow::FlockIntake;

#[cfg(feature = "flock")]
fn to_new_task(t: &newt_flock::NewTask) -> newt_core::NewFlockTask {
    newt_core::NewFlockTask {
        body: t.body.clone(),
        flock_cid: t.flock_cid.clone(),
        event_cid: t.event_cid.clone(),
        evidence_cid: t.evidence_cid.clone(),
        evidence: t.evidence.clone(),
    }
}
