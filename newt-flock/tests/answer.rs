//! Sending a draft answer to the operator's signer. The signer here is a
//! test double that records what arrives and replies as scripted; the real
//! one asks a human on a terminal, which newt can neither do nor fake.
#![cfg(unix)]

use content_addressable::ContentId;
use newt_flock::{view, Answerer, AskView, OpenAsk, Outcome, SignReply, SignRequest};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn id(n: u8) -> ContentId {
    // any valid content ids will do
    ContentId::from_dag_cbor_digest([n; 32])
}

fn ask(n: u8, text: &str) -> OpenAsk {
    OpenAsk {
        id: id(n),
        from: "h1".into(),
        lane: Some("l1".into()),
        text: text.into(),
    }
}

/// A signer double: serves `replies` in order, one connection each, and
/// records every request it receives.
struct Signer {
    _tmp: TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<SignRequest>>>,
    join: std::thread::JoinHandle<()>,
}

fn signer(replies: Vec<SignReply>) -> Signer {
    use agent_mesh_flock_server::wire::{read_msg, write_msg};
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sign.sock");
    let l = UnixListener::bind(&path).unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let log = Arc::clone(&seen);
    let join = std::thread::spawn(move || {
        for reply in replies {
            let (mut c, _) = l.accept().unwrap();
            let req: SignRequest = read_msg(&mut c).unwrap().unwrap();
            log.lock().unwrap().push(req);
            write_msg(&mut c, &reply).unwrap();
        }
    });
    Signer {
        _tmp: tmp,
        path,
        seen,
        join,
    }
}

#[test]
fn the_request_is_exactly_the_flock_the_ask_and_the_text_shown() {
    let s = signer(vec![SignReply::Admitted {
        event: id(9),
        index: 4,
    }]);
    let a = Answerer::new(id(1), s.path.clone());
    let q = ask(2, "which one?");
    let draft = a.draft(&q, "the blue one");
    let shown = (*draft.shown().0, draft.shown().1.to_owned());
    let out = a.send(draft).unwrap();
    s.join.join().unwrap();
    assert_eq!(
        *s.seen.lock().unwrap(),
        vec![SignRequest {
            flock: id(1),
            ask: shown.0,
            text: shown.1
        }]
    );
    assert_eq!(
        out,
        Outcome::Admitted {
            event: id(9),
            index: 4
        }
    );
}

#[test]
fn a_cancelled_draft_sends_nothing() {
    let s = signer(vec![]);
    let a = Answerer::new(id(1), s.path.clone());
    a.draft(&ask(2, "q"), "never sent").cancel();
    s.join.join().unwrap();
    assert!(s.seen.lock().unwrap().is_empty());
}

#[test]
fn a_declined_answer_comes_back_as_the_signers_refusal_and_is_not_retried() {
    let s = signer(vec![SignReply::Refused("declined".into())]);
    let a = Answerer::new(id(1), s.path.clone());
    let out = a.send(a.draft(&ask(2, "q"), "x")).unwrap();
    s.join.join().unwrap();
    assert_eq!(out, Outcome::Refused("declined".into()));
    assert_eq!(s.seen.lock().unwrap().len(), 1, "one attempt, no retry");
}

#[test]
fn no_signer_running_is_an_error_not_a_silent_success() {
    let tmp = TempDir::new().unwrap();
    let a = Answerer::new(id(1), tmp.path().join("nobody.sock"));
    assert!(matches!(
        a.send(a.draft(&ask(2, "q"), "x")),
        Err(newt_flock::SendError::NotSent(_))
    ));
}

#[test]
fn the_signers_word_is_not_enough_the_log_must_agree() {
    let q = id(2);
    let out = Outcome::Admitted {
        event: id(9),
        index: 4,
    };
    let mut v = AskView::default();
    assert!(!out.in_log(&q, &v), "not in the log yet");
    v.answered.insert(q, id(8));
    assert!(!out.in_log(&q, &v), "closed by a different event");
    v.answered.insert(q, id(9));
    assert!(out.in_log(&q, &v));
    assert!(!Outcome::Refused("no".into()).in_log(&q, &v));
}

/// Golden bytes: `write_msg` of `agent_mesh_cli::operator::SignRequest` and
/// `SignReply` (agent-mesh rev 8f1ee04066b7249999c6072888dd99a4630def68),
/// serialized once with those types for `flock = id(1)`, `ask = id(2)`,
/// `text = "t"`, `Admitted { event: id(9), index: 3 }` and `Refused("x")`.
/// Newt's own types must encode to the same bytes; repin both together.
const GOLD_REQUEST: &str = "00000064a36361736bd82a58250001711e2002020202020202020202020202020202020202020202020202020202020202026474657874617465666c6f636bd82a58250001711e200101010101010101010101010101010101010101010101010101010101010101";
const GOLD_ADMITTED: &str = "00000041a16841646d6974746564a2656576656e74d82a58250001711e20090909090909090909090909090909090909090909090909090909090909090965696e64657803";
const GOLD_REFUSED: &str = "0000000ba167526566757365646178";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_wire_format_is_the_signers_own() {
    use agent_mesh_flock_server::wire::write_msg;
    let mut a = vec![];
    write_msg(
        &mut a,
        &SignRequest {
            flock: id(1),
            ask: id(2),
            text: "t".into(),
        },
    )
    .unwrap();
    assert_eq!(hex(&a), GOLD_REQUEST);
    for (m, gold) in [
        (
            SignReply::Admitted {
                event: id(9),
                index: 3,
            },
            GOLD_ADMITTED,
        ),
        (SignReply::Refused("x".into()), GOLD_REFUSED),
    ] {
        let mut a = vec![];
        write_msg(&mut a, &m).unwrap();
        assert_eq!(hex(&a), gold);
    }
}

#[test]
fn newt_holds_no_key_and_has_no_approve_path() {
    // the crate has nothing that signs: no key type, no signing call, and the
    // only way to send a draft is `Answerer::send`, which a human-facing caller
    // invokes after reading `Draft::shown`.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for f in ["lib.rs", "answer.rs", "asks.rs"] {
        let text = std::fs::read_to_string(src.join(f)).unwrap();
        for banned in [
            "AgentKey",
            "SigningKey",
            "Assertion::sign",
            "signing_key",
            "seed",
        ] {
            assert!(!text.contains(banned), "{f} mentions {banned}");
        }
    }
    let _ = view; // the board view is read-only
}

#[test]
fn a_signer_that_hangs_up_without_answering_is_an_error_not_a_decision() {
    use agent_mesh_flock_server::wire::read_msg;
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sign.sock");
    let l = UnixListener::bind(&path).unwrap();
    let join = std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let _: Option<SignRequest> = read_msg(&mut c).unwrap();
        // drop the connection with no reply
    });
    let a = Answerer::new(id(1), path);
    assert!(matches!(
        a.send(a.draft(&ask(2, "q"), "x")),
        Err(newt_flock::SendError::Unknown(_))
    ));
    join.join().unwrap();
}
