//! What the operator sees of a flock task: where it came from, how far it
//! got, and what is still owed to the log. Every string from the log or the
//! session passes `inert`.

use agent_mesh_flock::text::inert;
use agent_mesh_flock::Board;

use crate::reply::{Replies, Sent, Step, ToolCaller};
use crate::task::{Evidence, TaskKind};

/// What newt itself knows about the task, from its own queue and turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// In the queue; not yet accepted.
    Queued,
    /// Accepted (receipt minted) and being worked.
    Running,
}

/// How far a task has come.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting in the queue.
    Queued,
    /// Accepted; the Ack is not yet in the log.
    Accepted,
    /// Accepted and acknowledged; being worked.
    Running,
    /// The Result or Verdict was sent and the log does not show it yet.
    ReplyPending(String),
    /// The log shows the reply.
    Replied,
    /// A reply was refused; it is not in the log.
    Refused(String),
}

/// One row for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    /// The flock.
    pub flock: String,
    /// This newt's member.
    pub member: String,
    /// The task's verified admission index.
    pub index: u64,
    /// Brief or review.
    pub kind: TaskKind,
    /// Who signed it, and their role in the verified roster.
    pub from: String,
    /// The signer's role, as the verified roster has it.
    pub from_role: Option<String>,
    /// The lane.
    pub lane: String,
    /// A review's exact head.
    pub head: Option<String>,
    /// How far it has come.
    pub phase: Phase,
}

/// The row for `ev`, from the verified `board`, newt's own `local` state and
/// the recorded replies.
#[must_use]
pub fn present<C: ToolCaller>(
    ev: &Evidence,
    board: &Board,
    local: Local,
    replies: &Replies<C>,
) -> TaskView {
    let state = |step| replies.state(&ev.flock, &ev.event, step);
    let phase = match (state(Step::Finish), state(Step::Ack), local) {
        (Some(Sent::Confirmed { .. }), _, _) => Phase::Replied,
        (Some(Sent::Refused { why }), _, _) => Phase::Refused(inert(why).into_owned()),
        (Some(Sent::Intent), _, _) => Phase::ReplyPending("sending".into()),
        (Some(Sent::Admitted { .. }), _, _) => Phase::ReplyPending("not yet in the log".into()),
        (Some(Sent::Ambiguous { why }), _, _) => {
            Phase::ReplyPending(format!("no clear answer: {}", inert(why)))
        }
        (None, _, Local::Queued) => Phase::Queued,
        (None, Some(Sent::Refused { why }), _) => Phase::Refused(format!("ack: {}", inert(why))),
        (None, Some(Sent::Confirmed { .. }), Local::Running) => Phase::Running,
        (None, _, Local::Running) => Phase::Accepted,
    };
    let from_role = board
        .members()
        .iter()
        .find(|m| m.name == ev.from)
        .map(|m| format!("{:?}", m.role).to_lowercase());
    TaskView {
        flock: ev.flock.clone(),
        member: replies.member().to_owned(),
        index: ev.index,
        kind: ev.kind,
        from: ev.from.clone(),
        from_role,
        lane: ev.lane.clone(),
        head: ev.head.clone(),
        phase,
    }
}

impl TaskView {
    /// One inert line.
    #[must_use]
    pub fn line(&self) -> String {
        let what = match self.kind {
            TaskKind::Brief => "brief",
            TaskKind::Review => "review",
        };
        let phase = match &self.phase {
            Phase::Queued => "queued".to_owned(),
            Phase::Accepted => "accepted (ack pending)".to_owned(),
            Phase::Running => "running".to_owned(),
            Phase::ReplyPending(w) => format!("reply pending ({w})"),
            Phase::Replied => "replied".to_owned(),
            Phase::Refused(w) => format!("reply refused ({w})"),
        };
        let head = self
            .head
            .as_deref()
            .map_or_else(String::new, |h| format!(" · head {}", inert(h)));
        format!(
            "{what} #{} · flock {} as {} · from {} ({}) · lane {}{head} · {phase}",
            self.index,
            inert(&self.flock),
            inert(&self.member),
            inert(&self.from),
            inert(self.from_role.as_deref().unwrap_or("not on the roster")),
            inert(&self.lane),
        )
    }
}
