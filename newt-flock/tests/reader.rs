//! The inbox against a real in-process flock server and a stand-in session
//! socket (test code may hold a key; the library may not). Waits are causal:
//! blocking channel reads, never sleeps.
#![cfg(unix)]

use agent_mesh_flock::{Body, Event, Kind, Member, Role};
use agent_mesh_flock_server::wire::{read_msg, write_msg, Req, Resp};
use agent_mesh_flock_server::{bind_private, init, load_genesis, serve, InitSpec, Server, Serving};
use agent_mesh_flock_session::{CtlReq, CtlResp};
use agent_mesh_protocol::{AgentKey, Assertion, UserKey};
use content_addressable::{ContentAddressable, ContentId};
use ed25519_dalek::{Signer, SigningKey};
use newt_flock::{Config, Ended, Error, Inbox, Observed};
use rand::rngs::OsRng;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn member(name: &str, role: Role) -> Member {
    Member {
        name: name.into(),
        role,
        devices: vec![],
        delegate: false,
    }
}

struct Rig {
    tmp: TempDir,
    sock: PathBuf,
    serving: Option<Serving>,
    at: u64,
}

fn rpc(c: &mut UnixStream, req: &Req) -> Resp {
    write_msg(c, req).unwrap();
    read_msg(c).unwrap().unwrap()
}

struct Enrolled {
    conn: UnixStream,
    seed: [u8; 32],
    cert: agent_mesh_protocol::CertChain,
    session: String,
}

impl Enrolled {
    fn key(&self) -> AgentKey {
        AgentKey::from_seed_and_cert(&self.seed, self.cert.clone()).unwrap()
    }
}

impl Rig {
    fn new() -> Self {
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
    fn genesis(&self) -> content_addressable::MerkleNode<agent_mesh_flock::Genesis> {
        load_genesis(self.tmp.path()).unwrap().node
    }
    fn conn(&self) -> UnixStream {
        UnixStream::connect(&self.sock).unwrap()
    }
    fn enrol(&self, name: &str) -> Enrolled {
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
    fn event(
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
    fn brief(&mut self, who: &mut Enrolled) {
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
    fn roster_of_shep_only(&mut self, who: &mut Enrolled) {
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
struct FakeSession {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl FakeSession {
    fn new(dir: &Path, flock: ContentId, name: &str, pending: bool, who: &Enrolled) -> Self {
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

fn config(r: &Rig, s: &FakeSession) -> Config {
    Config {
        genesis: r.genesis(),
        serve: r.sock.clone(),
        session: s.path.clone(),
        state_dir: r.tmp.path().join("newt-state"),
        retry: Duration::from_millis(20),
    }
}

fn session_for(r: &Rig, name: &str, who: &Enrolled) -> FakeSession {
    FakeSession::new(
        r.tmp.path(),
        r.genesis().content_id().unwrap(),
        name,
        false,
        who,
    )
}

/// Kinds seen until the replay is caught up.
fn until_caught(inbox: &Inbox) -> Vec<Kind> {
    let mut kinds = vec![];
    loop {
        match inbox.events().recv().unwrap() {
            Observed::Frame { frame, .. } => {
                kinds.extend(frame.iter().map(|s| s.event.payload.kind));
            }
            Observed::Caught(_) => return kinds,
            Observed::Unavailable(w) => panic!("unavailable: {w}"),
        }
    }
}

#[test]
fn it_follows_the_log_and_exposes_only_saved_frames() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    r.brief(&mut shep);
    let s = session_for(&r, "h1", &h1);
    let inbox = Inbox::start(config(&r, &s)).unwrap();
    let kinds = until_caught(&inbox);
    assert!(kinds.contains(&Kind::Brief), "{kinds:?}");
    // a later event arrives on a later pass
    r.brief(&mut shep);
    let more = until_caught(&inbox);
    assert_eq!(more.iter().filter(|k| **k == Kind::Brief).count(), 1);
    assert_eq!(inbox.stop(), Ended::Stopped);
}

#[test]
fn stop_joins_and_a_second_inbox_may_then_open_the_same_state() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    r.brief(&mut shep);
    let s = session_for(&r, "h1", &h1);
    let inbox = Inbox::start(config(&r, &s)).unwrap();
    until_caught(&inbox);
    assert_eq!(inbox.stop(), Ended::Stopped);
    // the state directory's lock was released by the join
    let again = Inbox::start(config(&r, &s)).unwrap();
    drop(again);
}

#[test]
fn a_session_of_another_flock_is_refused() {
    let r = Rig::new();
    let h1 = r.enrol("h1");
    let other = Rig::new();
    let s = FakeSession::new(
        r.tmp.path(),
        other.genesis().content_id().unwrap(),
        "h1",
        false,
        &h1,
    );
    assert!(matches!(
        Inbox::start(config(&r, &s)),
        Err(Error::WrongFlock)
    ));
}

#[test]
fn a_member_not_on_the_genesis_roster_is_refused() {
    let r = Rig::new();
    let h1 = r.enrol("h1");
    let s = session_for(&r, "ghost", &h1);
    assert!(matches!(
        Inbox::start(config(&r, &s)),
        Err(Error::NotAMember(m)) if m == "ghost"
    ));
}

#[test]
fn a_pending_replacement_is_refused() {
    let r = Rig::new();
    let h1 = r.enrol("h1");
    let s = FakeSession::new(
        r.tmp.path(),
        r.genesis().content_id().unwrap(),
        "h1",
        true,
        &h1,
    );
    assert!(matches!(Inbox::start(config(&r, &s)), Err(Error::Pending)));
}

#[test]
fn a_removed_member_reads_nothing_more_and_is_told_the_stream_is_unavailable() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    let s = session_for(&r, "h1", &h1);
    let inbox = Inbox::start(config(&r, &s)).unwrap();
    until_caught(&inbox);
    r.roster_of_shep_only(&mut shep);
    // no frame after the removal is exposed: the stream is refused instead
    loop {
        match inbox.events().recv().unwrap() {
            Observed::Unavailable(_) => break,
            Observed::Caught(_) => {}
            Observed::Frame { frame, .. } => {
                // only the roster that drops the member may still arrive
                assert!(frame.iter().all(|s| s.event.payload.kind == Kind::Roster));
            }
        }
    }
    assert!(matches!(inbox.stop(), Ended::Stopped | Ended::Removed));
}

fn started_with_state(r: &Rig, s: &FakeSession, dir: PathBuf) -> Result<Inbox, Error> {
    Inbox::start(Config {
        state_dir: dir,
        ..config(r, s)
    })
}

#[test]
fn a_state_directory_others_can_enter_is_refused_and_one_we_made_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let r = Rig::new();
    let h1 = r.enrol("h1");
    let s = session_for(&r, "h1", &h1);
    let loose = r.tmp.path().join("loose");
    std::fs::create_dir(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        started_with_state(&r, &s, loose.clone()),
        Err(Error::Reader(_))
    ));
    // the refusal did not loosen or tighten anything it does not own
    assert_eq!(
        std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let fresh = r.tmp.path().join("fresh");
    drop(started_with_state(&r, &s, fresh.clone()).unwrap());
    assert_eq!(
        std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // an existing private directory is fine
    drop(started_with_state(&r, &s, fresh).unwrap());
}

#[test]
fn a_state_directory_that_is_a_symlink_is_refused() {
    let r = Rig::new();
    let h1 = r.enrol("h1");
    let s = session_for(&r, "h1", &h1);
    let real = r.tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = r.tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        started_with_state(&r, &s, link),
        Err(Error::Reader(_))
    ));
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}
