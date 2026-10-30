//! The in-process flock server, enrolled members and a stand-in session
//! socket, shared by the reader and intake tests.
#![allow(dead_code)]

use agent_mesh_flock::{Body, Event, Kind, Member, Role};
use agent_mesh_flock_server::wire::{read_msg, write_msg, Req, Resp};
use agent_mesh_flock_server::{bind_private, init, load_genesis, serve, InitSpec, Server, Serving};
use agent_mesh_flock_session::{CtlReq, CtlResp};
use agent_mesh_protocol::{AgentKey, Assertion, UserKey};
use content_addressable::{ContentAddressable, ContentId};
use ed25519_dalek::{Signer, SigningKey};
use newt_flock::{Config, Inbox, Observed};
use rand::rngs::OsRng;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

pub fn member(name: &str, role: Role) -> Member {
    Member {
        name: name.into(),
        role,
        devices: vec![],
        delegate: false,
    }
}

pub struct Rig {
    pub tmp: TempDir,
    pub sock: PathBuf,
    pub serving: Option<Serving>,
    pub at: u64,
}

pub fn rpc(c: &mut UnixStream, req: &Req) -> Resp {
    write_msg(c, req).unwrap();
    read_msg(c).unwrap().unwrap()
}

pub struct Enrolled {
    pub conn: UnixStream,
    pub seed: [u8; 32],
    pub cert: agent_mesh_protocol::CertChain,
    pub session: String,
}

impl Enrolled {
    pub fn key(&self) -> AgentKey {
        AgentKey::from_seed_and_cert(&self.seed, self.cert.clone()).unwrap()
    }
}

impl Rig {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        init(
            tmp.path(),
            &UserKey::generate(),
            InitSpec {
                name: "f".into(),
                roster: vec![member("shep", Role::Shepherd), member("h1", Role::Helper)],
                salt: "00".into(),
                host: "h".into(),
                issued_at: "t".into(),
            },
        )
        .unwrap();
        let sock = tmp.path().join("run").join("serve.sock");
        let serving = serve(
            Server::open(tmp.path()).unwrap(),
            bind_private(&sock).unwrap(),
        )
        .unwrap();
        Self {
            tmp,
            sock,
            serving: Some(serving),
            at: 0,
        }
    }
    pub fn genesis(&self) -> content_addressable::MerkleNode<agent_mesh_flock::Genesis> {
        load_genesis(self.tmp.path()).unwrap().node
    }
    pub fn conn(&self) -> UnixStream {
        UnixStream::connect(&self.sock).unwrap()
    }
    pub fn enrol(&self, name: &str) -> Enrolled {
        let mut c = self.conn();
        let Resp::Invited { token } = rpc(
            &mut c,
            &Req::Invite {
                member: name.into(),
            },
        ) else {
            panic!()
        };
        let sk = SigningKey::generate(&mut OsRng);
        let Resp::Challenge(ch) = rpc(
            &mut c,
            &Req::EnrollBegin {
                token,
                member: name.into(),
                pubkey: sk.verifying_key().to_bytes(),
            },
        ) else {
            panic!()
        };
        let proof = sk.sign(&ch.signing_bytes());
        let Resp::Enrolled { session, cert, .. } = rpc(
            &mut c,
            &Req::EnrollFinish {
                challenge: ch,
                proof,
            },
        ) else {
            panic!()
        };
        Enrolled {
            conn: c,
            seed: sk.to_bytes(),
            cert,
            session,
        }
    }
    pub fn event(
        &mut self,
        who: &mut Enrolled,
        kind: Kind,
        refs: Vec<(String, ContentId)>,
        body: Body,
    ) {
        let Resp::Head(prev) = rpc(
            &mut who.conn,
            &Req::Head {
                session: who.session.clone(),
            },
        ) else {
            panic!()
        };
        let Resp::Hello { flock, .. } = rpc(&mut self.conn(), &Req::Hello) else {
            panic!()
        };
        self.at += 1;
        let node = Event {
            flock,
            kind,
            author: who.session.clone(),
            prev,
            refs: refs.into_iter().collect(),
            files: vec![],
            at: self.at,
            body,
        }
        .node();
        let sig = Assertion::sign(&who.key(), &node.content_id().unwrap());
        assert!(matches!(
            rpc(&mut who.conn, &Req::Admit { node, sig }),
            Resp::Admitted { .. }
        ));
    }
    pub fn brief(&mut self, who: &mut Enrolled) {
        self.event(
            who,
            Kind::Brief,
            vec![],
            Body::Brief {
                to: "h1".into(),
                lane: "a".into(),
            },
        );
    }
    pub fn roster_of_shep_only(&mut self, who: &mut Enrolled) {
        let replaces = self
            .serving
            .as_ref()
            .unwrap()
            .with_server(|s| s.board().roster_id());
        self.event(
            who,
            Kind::Roster,
            vec![("replaces".into(), replaces)],
            Body::Roster {
                members: vec![member("shep", Role::Shepherd)],
                roots: vec![],
            },
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.serving.take() {
            s.stop();
        }
    }
}

/// A stand-in for the member's session process: answers `Whoami` and signs
/// stream challenges with the held key.
pub struct FakeSession {
    pub path: PathBuf,
    pub stop: Arc<AtomicBool>,
    pub join: Option<std::thread::JoinHandle<()>>,
}

impl FakeSession {
    pub fn new(dir: &Path, flock: ContentId, name: &str, pending: bool, who: &Enrolled) -> Self {
        let path = dir.join("session.sock");
        let l = UnixListener::bind(&path).unwrap();
        l.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let key = who.key();
        let session = who.session.clone();
        let name = name.to_owned();
        let join = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match l.accept() {
                    Ok((mut c, _)) => {
                        c.set_nonblocking(false).unwrap();
                        let Some(req) = read_msg::<CtlReq>(&mut c).unwrap() else {
                            continue;
                        };
                        let resp = match req {
                            CtlReq::Whoami => CtlResp::Who {
                                flock,
                                member: name.clone(),
                                session: session.clone(),
                                pending,
                            },
                            CtlReq::ProveStream { challenge } => CtlResp::Proof(Box::new(
                                Assertion::sign(&key, &challenge.id().unwrap()),
                            )),
                            _ => CtlResp::Err("the stand-in session does not sign".into()),
                        };
                        write_msg(&mut c, &resp).unwrap();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            path,
            stop,
            join: Some(join),
        }
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn config(r: &Rig, s: &FakeSession) -> Config {
    Config {
        genesis: r.genesis(),
        serve: r.sock.clone(),
        session: s.path.clone(),
        state_dir: r.tmp.path().join("newt-state"),
        retry: Duration::from_millis(20),
    }
}

pub fn session_for(r: &Rig, name: &str, who: &Enrolled) -> FakeSession {
    FakeSession::new(
        r.tmp.path(),
        r.genesis().content_id().unwrap(),
        name,
        false,
        who,
    )
}

/// Kinds seen until the replay is caught up.
pub fn until_caught(inbox: &Inbox) -> Vec<Kind> {
    let mut kinds = vec![];
    loop {
        match inbox.events().recv().unwrap() {
            Observed::Frame { frame, .. } => {
                kinds.extend(frame.iter().map(|s| s.event.payload.kind));
            }
            Observed::Asks(_) => {}
            Observed::Caught(_) => return kinds,
            Observed::Unavailable(w) => panic!("unavailable: {w}"),
        }
    }
}

/// What the real session's socket does with an Emit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Sign and admit, and answer.
    None,
    /// Admit it, then close without answering: the caller cannot tell.
    DropAnswerAfterAdmit,
    /// Close without doing anything: nothing is admitted.
    DropBeforeAdmit,
}

/// A real member session (the key is held and used by Session, as in the
/// session process), served on a ctl socket by a thread, as the real process
/// does. A test sets `fault` to lose an answer.
pub struct RealSession {
    pub path: PathBuf,
    pub fault: Arc<std::sync::Mutex<Fault>>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RealSession {
    pub fn new(r: &Rig, name: &str) -> Self {
        use agent_mesh_flock_session::{Policy, Session};
        let mut c = r.conn();
        let Resp::Invited { token } = rpc(
            &mut c,
            &Req::Invite {
                member: name.into(),
            },
        ) else {
            panic!()
        };
        let mut s = Session::enroll(&r.sock, &token, name, Policy::AllowUnlocked).unwrap();
        let flock = r.genesis().content_id().unwrap();
        let dir = tempfile::Builder::new()
            .prefix("rs")
            .tempdir_in(r.tmp.path())
            .unwrap()
            .keep();
        let path = dir.join("session.sock");
        let l = UnixListener::bind(&path).unwrap();
        l.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let fault = Arc::new(std::sync::Mutex::new(Fault::None));
        let f2 = Arc::clone(&fault);
        let name = name.to_owned();
        let join = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let Ok((mut c, _)) = l.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                c.set_nonblocking(false).unwrap();
                let Some(req) = read_msg::<CtlReq>(&mut c).unwrap() else {
                    continue;
                };
                let resp = match req {
                    CtlReq::Whoami => CtlResp::Who {
                        flock,
                        member: name.clone(),
                        session: s.id().to_owned(),
                        pending: false,
                    },
                    CtlReq::Emit {
                        kind, refs, body, ..
                    } => {
                        let fault = *f2.lock().unwrap();
                        if fault == Fault::DropBeforeAdmit {
                            continue;
                        }
                        let out = s.emit(kind, refs, body);
                        if fault == Fault::DropAnswerAfterAdmit {
                            continue;
                        }
                        out.map_or_else(
                            |e| CtlResp::Err(e.to_string()),
                            |(event, index)| CtlResp::Admitted { event, index },
                        )
                    }
                    CtlReq::ProveStream { challenge } => s.prove_stream(&challenge).map_or_else(
                        |e| CtlResp::Err(e.to_string()),
                        |p| CtlResp::Proof(Box::new(p)),
                    ),
                    _ => CtlResp::Err("not served here".into()),
                };
                write_msg(&mut c, &resp).unwrap();
            }
            let _ = s.end();
        });
        Self {
            path,
            fault,
            stop,
            join: Some(join),
        }
    }
}

impl Drop for RealSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The MCP child's flock_emit, in-process: the same JSON in, the same words
/// out, over a real session socket. The tool parses kind, refs and body as
/// the flock types and makes one Emit; a refusal is an error result, a lost
/// connection is no answer.
pub struct SessionTool {
    pub sock: PathBuf,
    pub calls: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl newt_flock::ToolCaller for SessionTool {
    fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<newt_flock::ToolReply, newt_flock::Lost> {
        use newt_flock::{Lost, ToolReply};
        assert_eq!(name, "flock_emit");
        self.calls.lock().unwrap().push(args.clone());
        let kind = serde_json::from_value(args["kind"].clone());
        let refs = serde_json::from_value(args["refs"].clone());
        let body = serde_json::from_value(args["body"].clone());
        let (Ok(kind), Ok(refs), Ok(body)) = (kind, refs, body) else {
            return Ok(ToolReply::Error("the arguments are not an event".into()));
        };
        match agent_mesh_flock_session::call(
            &self.sock,
            &CtlReq::Emit {
                kind,
                refs,
                body,
                files: vec![],
            },
        ) {
            Ok(CtlResp::Admitted { event, index }) => {
                Ok(ToolReply::Text(format!("admitted {event} at {index}")))
            }
            Ok(_) => Ok(ToolReply::Error(
                "the session answered something else".into(),
            )),
            Err(agent_mesh_flock_session::SessionError::Refused(why)) => {
                Ok(ToolReply::Error(format!("refused: {why}")))
            }
            Err(e) => Err(Lost(e.to_string())),
        }
    }
}
