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
