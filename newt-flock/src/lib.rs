//! The read side of newt's flock inbox: follow a flock as one of its members
//! and expose only frames that were verified **and saved**.
//!
//! Newt holds no flock key. The member's session process signs the stream
//! challenge over its control socket; this crate never sees a key.
#![cfg(unix)]

mod answer;
mod asks;
mod task;

pub use answer::{Answerer, Draft, Outcome, SignReply, SignRequest};
pub use asks::{view, AskView, OpenAsk};
pub use task::{open_tasks, verify, CitedFile, Evidence, Intake, NewTask, Sink, Task, TaskKind};

/// The task text, for tests of its inertness.
#[doc(hidden)]
#[must_use]
pub fn render_for_test(ev: &Evidence) -> String {
    task::render(ev)
}

use agent_mesh_flock::Genesis;
use agent_mesh_flock_server::wire::{read_msg, write_msg, StreamChallenge};
use agent_mesh_flock_server::{private_dir, Follower, Sealed};
use agent_mesh_flock_session::{call, CtlReq, CtlResp, Reader, ReaderError, RealFs, Seen};
use agent_mesh_protocol::Assertion;
use content_addressable::{ContentAddressable, MerkleNode};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// What to follow, and as whom.
pub struct Config {
    /// The pinned genesis of the flock this newt belongs to.
    pub genesis: MerkleNode<Genesis>,
    /// The flock server's stream socket.
    pub serve: PathBuf,
    /// The member session's control socket.
    pub session: PathBuf,
    /// A private directory for the reader's checkpoint.
    pub state_dir: PathBuf,
    /// How long to wait before re-opening a stream that ended.
    pub retry: Duration,
}

/// Why the inbox did not start.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The session socket did not answer, or refused.
    #[error("session: {0}")]
    Session(String),
    /// The session belongs to another flock than the pinned genesis.
    #[error("the session is in a different flock than the pinned genesis")]
    WrongFlock,
    /// The session's member is not on the genesis roster.
    #[error("member {0:?} is not on the pinned genesis roster")]
    NotAMember(String),
    /// A replacement session that cannot sign yet.
    #[error("the session is a pending replacement and cannot sign yet")]
    Pending,
    /// The saved state or the first open failed.
    #[error("reader: {0}")]
    Reader(#[from] ReaderError),
}

/// A verified, saved step of the flock.
pub enum Observed {
    /// One committed frame, with the number of events admitted after it.
    Frame {
        /// The frame's sealed events.
        frame: Vec<Sealed>,
        /// Events admitted so far.
        admitted: u64,
    },
    /// The replay is over at this index.
    Caught(u64),
    /// The verified asks after a catch-up: open ones, and which event answered
    /// each closed one.
    Asks(AskView),
    /// The stream was refused or could not be opened (the server is away, or
    /// this member's session no longer reads); nothing was read this pass and
    /// the next is tried after `retry`.
    Unavailable(String),
}

/// Why the follower stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// [`Inbox::stop`] asked it to.
    Stopped,
    /// The roster no longer lists this member.
    Removed,
    /// The server's history rolled back or a frame did not verify.
    Failed(String),
}

/// Connect without waiting: a peer whose accept queue is full fails with
/// `WouldBlock` at once, where a blocking connect would wait for room with no
/// socket yet for `halt` to shut down. The pass then ends as `Unavailable` and
/// is retried after `retry`.
fn connect(path: &Path) -> std::io::Result<UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    sock.set_nonblocking(true)?;
    sock.connect(&SockAddr::unix(path)?)?;
    sock.set_nonblocking(false)?;
    Ok(UnixStream::from(std::os::fd::OwnedFd::from(sock)))
}

/// The sockets a worker is blocked on, so [`Inbox::stop`] can shut them down:
/// a read the peer never answers then fails and the worker sees the stop flag.
#[derive(Default)]
struct Halt {
    stopped: AtomicBool,
    open: Mutex<Vec<UnixStream>>,
}

impl Halt {
    /// Keep a handle to `s` so `halt` can shut it down; if halt has already
    /// run, shut it down now (a connection made after the stop is dead).
    fn track(&self, s: &UnixStream) -> std::io::Result<()> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let clone = s.try_clone()?;
        if self.stopped.load(Ordering::SeqCst) {
            let _ = clone.shutdown(std::net::Shutdown::Both);
        } else {
            open.push(clone);
        }
        Ok(())
    }

    /// The pass is over: drop the handles (they would otherwise pile up).
    fn release(&self) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn halt(&self) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        self.stopped.store(true, Ordering::SeqCst);
        for c in open.iter() {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Ask the session to sign `challenge`, over a connection `halt` can cut.
    fn prove(&self, session: &Path, challenge: &StreamChallenge) -> Result<Assertion, ReaderError> {
        let stream = |e: String| ReaderError::Stream(e);
        let mut s = connect(session).map_err(|e| stream(e.to_string()))?;
        self.track(&s).map_err(|e| stream(e.to_string()))?;
        let req = CtlReq::ProveStream {
            challenge: challenge.clone(),
        };
        write_msg(&mut s, &req).map_err(|e| stream(e.to_string()))?;
        match read_msg::<CtlResp>(&mut s).map_err(|e| stream(e.to_string()))? {
            Some(CtlResp::Proof(p)) => Ok(*p),
            Some(CtlResp::Err(why)) => Err(stream(why)),
            _ => Err(stream("the session answered something else".into())),
        }
    }
}

/// A running follower. Dropping it stops and joins the worker.
pub struct Inbox {
    rx: Receiver<Observed>,
    halt: Arc<Halt>,
    thread: Option<JoinHandle<Ended>>,
}

impl Inbox {
    /// Check the session against the genesis, open the saved state, and start
    /// following.
    ///
    /// # Errors
    /// See [`Error`].
    pub fn start(cfg: Config) -> Result<Self, Error> {
        Self::start_with(cfg, |_| {})
    }

    /// Follow with a callback and nothing else: like [`start_with`](Self::start_with)
    /// but no observation is ever buffered, so [`events`](Self::events) is
    /// empty and closed. How the worker ended is still reported by
    /// [`stop`](Self::stop); a pass that finds the server away is retried.
    ///
    /// # Errors
    /// See [`Error`].
    pub fn follow(
        cfg: Config,
        on_state: impl FnMut(&Follower) + Send + 'static,
    ) -> Result<Self, Error> {
        Self::launch(cfg, on_state, false)
    }

    /// [`start`](Self::start), and `on_state` is called on the worker with the
    /// verified follower: once for the board the reader saved earlier (so
    /// work that arrived before a restart is seen even if no new frame ever
    /// comes) and again at each catch-up, always after the reader has saved.
    /// Never for a frame in the middle of a replay: a prefix of the log shows
    /// work as open that a later frame closes. It never sees anything the
    /// reader did not verify.
    ///
    /// # Errors
    /// See [`Error`].
    pub fn start_with(
        cfg: Config,
        on_state: impl FnMut(&Follower) + Send + 'static,
    ) -> Result<Self, Error> {
        Self::launch(cfg, on_state, true)
    }

    fn launch(
        cfg: Config,
        mut on_state: impl FnMut(&Follower) + Send + 'static,
        observe: bool,
    ) -> Result<Self, Error> {
        match call(&cfg.session, &CtlReq::Whoami).map_err(|e| Error::Session(e.to_string()))? {
            CtlResp::Who {
                flock,
                member,
                pending,
                ..
            } => {
                let id = cfg
                    .genesis
                    .content_id()
                    .map_err(|e| Error::Session(e.to_string()))?;
                if flock != id {
                    return Err(Error::WrongFlock);
                }
                if !cfg.genesis.payload.roster.iter().any(|m| m.name == member) {
                    return Err(Error::NotAMember(member));
                }
                if pending {
                    return Err(Error::Pending);
                }
                // ours, a real directory, closed to others (a loose or linked one is refused)
                private_dir(&cfg.state_dir).map_err(ReaderError::Io)?;
                let mut reader = Reader::open(cfg.genesis.clone(), &cfg.state_dir, RealFs)?;
                let (tx, rx) = channel();
                // callback-only: nothing is ever sent, and the channel is closed
                let tx = observe.then_some(tx);
                let send = move |tx: &Option<Sender<Observed>>, o: Observed| {
                    tx.as_ref().is_some_and(|t| t.send(o).is_err())
                };
                let halt = Arc::new(Halt::default());
                let flag = Arc::clone(&halt);
                let thread = thread::spawn(move || {
                    on_state(reader.follower());
                    let prove = |c: &StreamChallenge| flag.prove(&cfg.session, c);
                    // ponytail: each pass is a catch-up stream, re-opened after
                    // `retry`; the live tail waits for the next pass. One long
                    // stream (`once = false`) is possible now that `halt` can
                    // cut a stream from outside.
                    while !flag.is_stopped() {
                        let mut removed = false;
                        let mut gone = false;
                        let conn = connect(&cfg.serve)
                            .map_err(ReaderError::Io)
                            .and_then(|c| flag.track(&c).map(|()| c).map_err(ReaderError::Io));
                        let r = conn.and_then(|conn| {
                            reader.follow_on(conn, &prove, true, &mut |s| match s {
                                Seen::Frame(f, fol) => {
                                    removed =
                                        !fol.board().members().iter().any(|m| m.name == member);
                                    gone |= send(
                                        &tx,
                                        Observed::Frame {
                                            frame: f.to_vec(),
                                            admitted: fol.admitted(),
                                        },
                                    );
                                }
                                Seen::Caught(i, fol) => {
                                    on_state(fol);
                                    removed =
                                        !fol.board().members().iter().any(|m| m.name == member);
                                    gone |= send(&tx, Observed::Caught(i));
                                    if tx.is_some() {
                                        gone |= send(&tx, Observed::Asks(view(fol.board())));
                                    }
                                }
                            })
                        });
                        flag.release();
                        // a read cut by `stop` is not the server's doing
                        if flag.is_stopped() {
                            return Ended::Stopped;
                        }
                        if removed {
                            return Ended::Removed;
                        }
                        if gone {
                            return Ended::Stopped;
                        }
                        match r {
                            Err(ReaderError::Rollback(w) | ReaderError::Invalid(w)) => {
                                return Ended::Failed(w)
                            }
                            Err(ReaderError::Save(e)) => return Ended::Failed(e.to_string()),
                            Err(e @ (ReaderError::Stream(_) | ReaderError::Io(_)))
                                if send(&tx, Observed::Unavailable(e.to_string())) =>
                            {
                                return Ended::Stopped;
                            }
                            _ => {}
                        }
                        thread::park_timeout(cfg.retry);
                    }
                    Ended::Stopped
                });
                Ok(Self {
                    rx,
                    halt,
                    thread: Some(thread),
                })
            }
            _ => Err(Error::Session("unexpected answer".into())),
        }
    }

    /// The saved steps, in order.
    #[must_use]
    pub fn events(&self) -> &Receiver<Observed> {
        &self.rx
    }

    /// Stop the worker and wait for it; says why it ended.
    pub fn stop(mut self) -> Ended {
        self.halt()
    }

    fn halt(&mut self) -> Ended {
        self.halt.halt();
        match self.thread.take() {
            Some(t) => {
                t.thread().unpark();
                t.join()
                    .unwrap_or_else(|_| Ended::Failed("worker panicked".into()))
            }
            None => Ended::Stopped,
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.halt();
    }
}
