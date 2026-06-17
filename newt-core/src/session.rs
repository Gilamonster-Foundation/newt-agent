use crate::caveats::Caveats;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// Opaque session identifier — wraps a UUID v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

// ===========================================================================
// Multi-attach session model — Phase 1 of the mesh-remote control plane
// (docs/design/mesh-remote-control-mobile-app.md §7.1).
//
// The agent is NOT a single-stdin/stdout process. A *session* is a unit of work;
// a *terminal attach* — the local console OR a remote mesh peer — is just an
// `Attachment` to it. The same `OutputChunk` stream fans out to every attachment,
// so a human at the keyboard and a phone observing/co-driving see the same turn.
//
// This module is the in-process architecture and authority/arbitration logic; it
// has NO transport dependency. The mesh wire protocol (`newt/session/v1`,
// Phase 1b) and the live-console rewiring (local console becomes "attachment #0")
// sit on top of these types. Two invariants it enforces structurally:
//   * only a `Driver` attachment can submit input (an `Observer` never mutates);
//   * turns are serialized — a turn in flight must complete or cancel before the
//     next input is accepted, so two attachments can't interleave half-turns.
// Authority for a turn is the *active driver attachment's* caveats (each a
// worker-delegated down-set, so already ⊑ the worker — §5.1).
// ===========================================================================

/// Which logical stream an [`OutputChunk`] belongs to (a terminal styles each
/// distinctly — stdout vs. the agent's thoughts vs. a tool call vs. a diff).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    Stdout,
    Stderr,
    AgentThought,
    ToolCall,
    Diff,
}

/// One streamed unit of session output, fanned out to every attachment. This is
/// the in-process Phase-1 shape; the mesh wire `OutputChunk` (§6.1) maps onto it
/// 1:1 (it adds only `session_id`, carried by the topic in Phase 1b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    /// Which turn produced this chunk (monotonic within a session).
    pub turn: u64,
    pub stream: OutputStream,
    /// Ordering within the turn; an attachment dedupes/reorders by this and asks
    /// for a `replay_from(seq)` after a reconnect (§6.3).
    pub seq: u64,
    pub data: String,
    /// Final chunk of this turn's stream. Does NOT itself end the turn — the
    /// driver calls [`SessionState::complete_turn`] (a turn may stream several
    /// `last`-tagged sub-streams).
    pub last: bool,
}

/// How an attachment relates to its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    /// May submit input (drive turns). Its caveats govern the turns it drives.
    Driver,
    /// Read-only: receives the output stream, can never submit input. Observing
    /// can never mutate — enforced here, not by the observer's caveats.
    Observer,
}

/// Identifies one attachment within a session (assigned at attach time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct AttachId(u64);

impl fmt::Display for AttachId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "attach#{}", self.0)
    }
}

/// The sink an attachment receives [`OutputChunk`]s on. A local console, a mesh
/// peer, or a test collector all implement this, so the session fans out to
/// `dyn OutputSink` and never knows the transport.
pub trait OutputSink: Send {
    fn deliver(&mut self, chunk: &OutputChunk);
}

/// Why [`SessionState::submit_input`] refused an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefused {
    /// The attachment is an `Observer` — only a `Driver` may submit.
    NotADriver,
    /// A turn is already in flight; it must complete or cancel first.
    TurnInFlight,
    /// No such attachment in this session.
    NoSuchAttachment,
}

impl fmt::Display for InputRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotADriver => write!(
                f,
                "attachment is an observer; only a driver may submit input"
            ),
            Self::TurnInFlight => write!(
                f,
                "a turn is already in flight; complete or cancel it first"
            ),
            Self::NoSuchAttachment => write!(f, "no such attachment in this session"),
        }
    }
}

impl std::error::Error for InputRefused {}

struct Attachment {
    role: AttachRole,
    caveats: Caveats,
    sink: Box<dyn OutputSink>,
}

/// A turn accepted for execution: the harness runs it under `caveats` (the active
/// driver's authority) and streams output back via [`SessionState::emit`].
#[derive(Debug, Clone)]
pub struct AcceptedTurn {
    pub turn: u64,
    pub driver: AttachId,
    pub text: String,
    pub caveats: Caveats,
}

struct InFlight {
    turn: u64,
    driver: AttachId,
    next_seq: u64,
}

/// One session: a set of attachments fanned the same output, with serialized
/// input and per-driver authority.
pub struct SessionState {
    id: SessionId,
    attachments: BTreeMap<AttachId, Attachment>,
    next_attach: u64,
    turn: u64,
    in_flight: Option<InFlight>,
    ring: VecDeque<OutputChunk>,
    ring_cap: usize,
}

impl SessionState {
    /// `ring_cap` bounds the per-session resume buffer (recent chunks replayed to
    /// an attachment that reconnects — §6.3). Clamped to at least 1.
    pub fn new(id: SessionId, ring_cap: usize) -> Self {
        Self {
            id,
            attachments: BTreeMap::new(),
            next_attach: 0,
            turn: 0,
            in_flight: None,
            ring: VecDeque::new(),
            ring_cap: ring_cap.max(1),
        }
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Attach a console/peer/observer; returns its handle. The caller supplies the
    /// attachment's caveats — for a `Driver`, its worker-delegated down-set; for
    /// an `Observer`, an attenuated read-only set (though the role, not the
    /// caveats, is what structurally prevents an observer from mutating).
    pub fn attach(
        &mut self,
        role: AttachRole,
        caveats: Caveats,
        sink: Box<dyn OutputSink>,
    ) -> AttachId {
        let id = AttachId(self.next_attach);
        self.next_attach += 1;
        self.attachments.insert(
            id,
            Attachment {
                role,
                caveats,
                sink,
            },
        );
        id
    }

    /// Detach an attachment; future output no longer fans out to it. Returns
    /// whether it was present.
    pub fn detach(&mut self, id: AttachId) -> bool {
        self.attachments.remove(&id).is_some()
    }

    pub fn attachment_count(&self) -> usize {
        self.attachments.len()
    }

    pub fn driver_count(&self) -> usize {
        self.attachments
            .values()
            .filter(|a| a.role == AttachRole::Driver)
            .count()
    }

    pub fn turn_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The caveats governing the in-flight turn (the active driver's), or `None`
    /// when idle. A turn never runs under more authority than the driver that
    /// submitted it.
    pub fn effective_caveats(&self) -> Option<&Caveats> {
        let f = self.in_flight.as_ref()?;
        self.attachments.get(&f.driver).map(|a| &a.caveats)
    }

    /// Submit a human/peer input. Enforces the two structural invariants:
    /// driver-only, and one turn at a time. On accept, opens a new turn and
    /// returns it (with the active driver's caveats) for the harness to run.
    pub fn submit_input(
        &mut self,
        from: AttachId,
        text: impl Into<String>,
    ) -> Result<AcceptedTurn, InputRefused> {
        let att = self
            .attachments
            .get(&from)
            .ok_or(InputRefused::NoSuchAttachment)?;
        if att.role != AttachRole::Driver {
            return Err(InputRefused::NotADriver);
        }
        if self.in_flight.is_some() {
            return Err(InputRefused::TurnInFlight);
        }
        self.turn += 1;
        let caveats = att.caveats.clone();
        self.in_flight = Some(InFlight {
            turn: self.turn,
            driver: from,
            next_seq: 0,
        });
        Ok(AcceptedTurn {
            turn: self.turn,
            driver: from,
            text: text.into(),
            caveats,
        })
    }

    /// Emit one output chunk for the in-flight turn: buffer it (bounded by
    /// `ring_cap`) and fan it out to every attachment. No-op if idle.
    pub fn emit(&mut self, stream: OutputStream, data: impl Into<String>, last: bool) {
        let Some(f) = self.in_flight.as_mut() else {
            return;
        };
        let chunk = OutputChunk {
            turn: f.turn,
            stream,
            seq: f.next_seq,
            data: data.into(),
            last,
        };
        f.next_seq += 1;
        if self.ring.len() == self.ring_cap {
            self.ring.pop_front();
        }
        self.ring.push_back(chunk.clone());
        for att in self.attachments.values_mut() {
            att.sink.deliver(&chunk);
        }
    }

    /// End the in-flight turn (success). Returns whether one was in flight.
    pub fn complete_turn(&mut self) -> bool {
        self.in_flight.take().is_some()
    }

    /// Cancel the in-flight turn (a `ControlC` from any attachment). Returns
    /// whether one was in flight.
    pub fn cancel_turn(&mut self) -> bool {
        self.in_flight.take().is_some()
    }

    /// Replay buffered chunks with `seq >= from_seq` to a reconnecting attachment
    /// (§6.3 resume). Bounded by `ring_cap`, so a long-gone attachment may miss
    /// the head — it gets the retained tail.
    pub fn replay_from(&self, from_seq: u64) -> Vec<OutputChunk> {
        self.ring
            .iter()
            .filter(|c| c.seq >= from_seq)
            .cloned()
            .collect()
    }
}

/// The agent's session registry: `SessionId -> SessionState`. The core owns one;
/// every attach (local console or mesh peer) registers a session against it.
pub struct SessionRegistry {
    sessions: BTreeMap<SessionId, SessionState>,
    default_ring_cap: usize,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            default_ring_cap: 256,
        }
    }

    /// Registry whose sessions use `cap` as their resume-buffer size.
    pub fn with_ring_cap(cap: usize) -> Self {
        Self {
            sessions: BTreeMap::new(),
            default_ring_cap: cap.max(1),
        }
    }

    /// Open a fresh session; returns its id.
    pub fn open(&mut self) -> SessionId {
        let id = SessionId::new();
        self.sessions
            .insert(id, SessionState::new(id, self.default_ring_cap));
        id
    }

    /// Close a session, dropping all its attachments. Returns whether it existed.
    pub fn close(&mut self, id: SessionId) -> bool {
        self.sessions.remove(&id).is_some()
    }

    pub fn get(&self, id: SessionId) -> Option<&SessionState> {
        self.sessions.get(&id)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut SessionState> {
        self.sessions.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn ids(&self) -> Vec<SessionId> {
        self.sessions.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn parse_roundtrip() {
        let id = SessionId::new();
        let s = id.to_string();
        let parsed: SessionId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn display_is_uuid_format() {
        let id = SessionId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 36);
        assert_eq!(s.as_bytes()[8], b'-');
        assert_eq!(s.as_bytes()[13], b'-');
        assert_eq!(s.as_bytes()[18], b'-');
        assert_eq!(s.as_bytes()[23], b'-');
    }

    #[test]
    fn invalid_string_rejected() {
        assert!("not-a-uuid".parse::<SessionId>().is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}

#[cfg(test)]
mod attach_tests {
    use super::*;
    use crate::caveats::{CaveatsExt, Scope};
    use std::sync::{Arc, Mutex};

    /// An in-memory attachment sink that records everything fanned to it.
    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<OutputChunk>>>);

    impl OutputSink for Collector {
        fn deliver(&mut self, chunk: &OutputChunk) {
            self.0.lock().unwrap().push(chunk.clone());
        }
    }

    impl Collector {
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    /// A read-only observer authority: no exec (the role is the real gate, but
    /// this models the attenuated caveats §7.1 says an observer carries).
    fn observer_caveats() -> Caveats {
        Caveats {
            exec: Scope::none(),
            ..Caveats::top()
        }
    }

    #[test]
    fn registry_open_close_and_ids() {
        let mut reg = SessionRegistry::new();
        assert!(reg.is_empty());
        let a = reg.open();
        let b = reg.open();
        assert_eq!(reg.len(), 2);
        assert_ne!(a, b);
        assert!(reg.get(a).is_some());
        assert!(reg.ids().contains(&a) && reg.ids().contains(&b));
        assert!(reg.close(a));
        assert!(!reg.close(a)); // already gone
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn separate_sessions_are_isolated() {
        // The default attach mode: two drivers each on their OWN session never
        // see each other's output (§7.1 separate-sessions).
        let mut reg = SessionRegistry::new();
        let s1 = reg.open();
        let s2 = reg.open();
        let c1 = Collector::default();
        let c2 = Collector::default();
        let d1 = reg.get_mut(s1).unwrap().attach(
            AttachRole::Driver,
            Caveats::top(),
            Box::new(c1.clone()),
        );
        let _d2 = reg.get_mut(s2).unwrap().attach(
            AttachRole::Driver,
            Caveats::top(),
            Box::new(c2.clone()),
        );

        let sess1 = reg.get_mut(s1).unwrap();
        sess1.submit_input(d1, "go").unwrap();
        sess1.emit(OutputStream::Stdout, "only s1", true);

        assert_eq!(c1.count(), 1);
        assert_eq!(c2.count(), 0, "s2 must not see s1's output");
    }

    #[test]
    fn observer_receives_output_but_cannot_drive() {
        let mut reg = SessionRegistry::new();
        let s = reg.open();
        let cd = Collector::default();
        let co = Collector::default();
        let sess = reg.get_mut(s).unwrap();
        let driver = sess.attach(AttachRole::Driver, Caveats::top(), Box::new(cd.clone()));
        let observer = sess.attach(
            AttachRole::Observer,
            observer_caveats(),
            Box::new(co.clone()),
        );

        // The observer structurally cannot submit input.
        assert!(matches!(
            sess.submit_input(observer, "mutate"),
            Err(InputRefused::NotADriver)
        ));
        assert!(!sess.turn_in_flight());

        // The driver drives; output fans to BOTH the driver and the observer.
        sess.submit_input(driver, "go").unwrap();
        sess.emit(OutputStream::AgentThought, "thinking", false);
        sess.emit(OutputStream::Stdout, "done", true);

        assert_eq!(cd.count(), 2);
        assert_eq!(co.count(), 2, "observer watches the same stream");
    }

    #[test]
    fn turns_are_serialized_until_complete_or_cancel() {
        let mut reg = SessionRegistry::new();
        let s = reg.open();
        let sess = reg.get_mut(s).unwrap();
        let d = sess.attach(
            AttachRole::Driver,
            Caveats::top(),
            Box::new(Collector::default()),
        );

        let t1 = sess.submit_input(d, "a").unwrap();
        assert_eq!(t1.turn, 1);
        assert!(sess.turn_in_flight());

        // No second turn may start while one is in flight.
        assert!(matches!(
            sess.submit_input(d, "b"),
            Err(InputRefused::TurnInFlight)
        ));

        assert!(sess.complete_turn());
        let t2 = sess.submit_input(d, "b").unwrap();
        assert_eq!(t2.turn, 2, "turns increment monotonically");

        assert!(sess.cancel_turn());
        assert!(!sess.turn_in_flight());
        assert!(!sess.complete_turn(), "nothing in flight to complete");
    }

    #[test]
    fn co_drivers_share_output_and_serialize() {
        // Shared/observed session with two DRIVERS (e.g. laptop + phone co-drive):
        // output fans to both, and inputs still serialize across attachments.
        let mut reg = SessionRegistry::new();
        let s = reg.open();
        let ca = Collector::default();
        let cb = Collector::default();
        let sess = reg.get_mut(s).unwrap();
        let a = sess.attach(AttachRole::Driver, Caveats::top(), Box::new(ca.clone()));
        let b = sess.attach(AttachRole::Driver, Caveats::top(), Box::new(cb.clone()));
        assert_eq!(sess.driver_count(), 2);

        // A drives; B's input is refused mid-turn (no interleaving half-turns).
        sess.submit_input(a, "a-input").unwrap();
        assert!(matches!(
            sess.submit_input(b, "b-input"),
            Err(InputRefused::TurnInFlight)
        ));
        sess.emit(OutputStream::Stdout, "from a's turn", true);
        sess.complete_turn();

        // Now B can drive; both still see the output.
        sess.submit_input(b, "b-input").unwrap();
        sess.emit(OutputStream::Stdout, "from b's turn", true);
        sess.complete_turn();

        assert_eq!(ca.count(), 2, "A sees both turns' output");
        assert_eq!(cb.count(), 2, "B sees both turns' output");
    }

    #[test]
    fn effective_caveats_tracks_the_active_driver() {
        let mut reg = SessionRegistry::new();
        let s = reg.open();
        let sess = reg.get_mut(s).unwrap();
        // A: full authority. B: no exec.
        let a = sess.attach(
            AttachRole::Driver,
            Caveats::top(),
            Box::new(Collector::default()),
        );
        let b = sess.attach(
            AttachRole::Driver,
            observer_caveats(),
            Box::new(Collector::default()),
        );

        assert!(sess.effective_caveats().is_none(), "idle → no authority");

        sess.submit_input(a, "x").unwrap();
        assert!(
            sess.effective_caveats().unwrap().permits_exec("git"),
            "A's turn runs under A's (full) authority"
        );
        sess.complete_turn();

        sess.submit_input(b, "y").unwrap();
        assert!(
            !sess.effective_caveats().unwrap().permits_exec("git"),
            "B's turn runs under B's (no-exec) authority — the active driver governs"
        );
        sess.complete_turn();
        assert!(sess.effective_caveats().is_none());
    }

    #[test]
    fn replay_returns_the_buffered_tail() {
        let mut reg = SessionRegistry::with_ring_cap(3);
        let s = reg.open();
        let sess = reg.get_mut(s).unwrap();
        let d = sess.attach(
            AttachRole::Driver,
            Caveats::top(),
            Box::new(Collector::default()),
        );
        sess.submit_input(d, "go").unwrap();
        for i in 0..5 {
            sess.emit(OutputStream::Stdout, format!("chunk{i}"), false);
        }
        // ring_cap=3 keeps seq 2,3,4 (head evicted).
        let from0 = sess.replay_from(0);
        assert_eq!(
            from0.iter().map(|c| c.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        let from3 = sess.replay_from(3);
        assert_eq!(from3.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn detach_stops_fanout() {
        let mut reg = SessionRegistry::new();
        let s = reg.open();
        let cd = Collector::default();
        let co = Collector::default();
        let sess = reg.get_mut(s).unwrap();
        let d = sess.attach(AttachRole::Driver, Caveats::top(), Box::new(cd.clone()));
        let obs = sess.attach(
            AttachRole::Observer,
            observer_caveats(),
            Box::new(co.clone()),
        );

        assert_eq!(sess.attachment_count(), 2);
        assert!(sess.detach(obs));
        assert_eq!(sess.attachment_count(), 1);

        sess.submit_input(d, "go").unwrap();
        sess.emit(OutputStream::Stdout, "after detach", true);
        assert_eq!(cd.count(), 1);
        assert_eq!(co.count(), 0, "detached attachment receives nothing");
    }
}
