//! The read side of newt's flock inbox: follow a flock as one of its members
//! and expose only frames that were verified **and saved**.
//!
//! Newt holds no flock key. The member's session process signs the stream
//! challenge over its control socket; this crate never sees a key.
#![cfg(unix)]

use agent_mesh_flock::Genesis;
use agent_mesh_flock_server::{private_dir, Sealed};
use agent_mesh_flock_session::{
    call, session_prover, CtlReq, CtlResp, Reader, ReaderError, RealFs, Seen,
};
use content_addressable::{ContentAddressable, MerkleNode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
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

/// A running follower. Dropping it stops and joins the worker.
pub struct Inbox {
    rx: Receiver<Observed>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Ended>>,
}

impl Inbox {
    /// Check the session against the genesis, open the saved state, and start
    /// following.
    ///
    /// # Errors
    /// See [`Error`].
    pub fn start(cfg: Config) -> Result<Self, Error> {
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
                let stop = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&stop);
                let thread = thread::spawn(move || {
                    let prove = session_prover(&cfg.session);
                    // ponytail: each pass is a catch-up stream, re-opened after
                    // `retry`; the live tail waits for the next pass. Upgrade
                    // to one long stream once `Watch` can be shut down from
                    // outside.
                    while !flag.load(Ordering::SeqCst) {
                        let mut removed = false;
                        let mut gone = false;
                        let r = reader.follow(&cfg.serve, &prove, true, &mut |s| match s {
                            Seen::Frame(f, fol) => {
                                removed = !fol.board().members().iter().any(|m| m.name == member);
                                gone |= tx
                                    .send(Observed::Frame {
                                        frame: f.to_vec(),
                                        admitted: fol.admitted(),
                                    })
                                    .is_err();
                            }
                            Seen::Caught(i, fol) => {
                                removed = !fol.board().members().iter().any(|m| m.name == member);
                                gone |= tx.send(Observed::Caught(i)).is_err();
                            }
                        });
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
                                if tx.send(Observed::Unavailable(e.to_string())).is_err() =>
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
                    stop,
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
        self.stop.store(true, Ordering::SeqCst);
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
