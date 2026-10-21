//! `/asks` and `/answer` through the chat's command seam, with a signer
//! double on a real socket. Flock-origin input must not reach either.

use super::*;
use crate::flock_ui::{command, FlockUi};
use agent_mesh_flock_server::wire::{read_msg, write_msg};
use content_addressable::ContentId;
use newt_flock::{Answerer, AskView, OpenAsk, SignReply, SignRequest};
use std::os::unix::net::UnixListener;
use std::sync::{mpsc, Arc, Mutex};

fn id(n: u8) -> ContentId {
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

/// A signer that takes one request, runs `before_reply`, then answers.
struct Signer {
    _tmp: tempfile::TempDir,
    path: std::path::PathBuf,
    seen: Arc<Mutex<Vec<SignRequest>>>,
    done: mpsc::Receiver<()>,
}

fn signer_with(reply: SignReply, before_reply: impl FnOnce() + Send + 'static) -> Signer {
    serve(Some(reply), before_reply)
}

/// A signer that reads the request and hangs up without a decision.
fn hangup_with(before_hangup: impl FnOnce() + Send + 'static) -> Signer {
    serve(None, before_hangup)
}

fn serve(reply: Option<SignReply>, before_reply: impl FnOnce() + Send + 'static) -> Signer {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("sign.sock");
    let l = UnixListener::bind(&path).unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let log = Arc::clone(&seen);
    let (tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let req: SignRequest = read_msg(&mut c).unwrap().unwrap();
        log.lock().unwrap().push(req);
        before_reply();
        if let Some(reply) = reply {
            write_msg(&mut c, &reply).unwrap();
        }
        drop(c);
        let _ = tx.send(());
    });
    Signer {
        _tmp: tmp,
        path,
        seen,
        done,
    }
}

fn signer(reply: SignReply) -> Signer {
    signer_with(reply, || {})
}

fn ui(path: std::path::PathBuf, v: AskView) -> FlockUi {
    let ui = FlockUi::new(Answerer::new(id(200), path));
    ui.observe(v);
    ui
}

fn open(asks: Vec<OpenAsk>) -> AskView {
    AskView {
        open: asks,
        ..AskView::default()
    }
}

/// What was shown before the command finished, then what it returned.
fn run_split(ui: &FlockUi, line: &str) -> (Vec<String>, Vec<String>) {
    let mut shown = vec![];
    let back =
        command(true, line, Some(ui), &mut |l| shown.push(l.to_owned())).expect("a flock command");
    (shown, back)
}

fn run(ui: &FlockUi, line: &str) -> String {
    let (mut all, back) = run_split(ui, line);
    all.extend(back);
    all.join("\n")
}

#[test]
fn asks_are_numbered_and_hostile_text_cannot_forge_output() {
    let ui = ui(
        "/none".into(),
        open(vec![ask(1, "ok?"), ask(2, "x\u{1b}[2J\nforged line")]),
    );
    let out = run(&ui, "/asks");
    assert!(out.starts_with("1. "), "{out}");
    assert!(out.contains("\n2. "), "{out}");
    assert!(
        !out.contains('\u{1b}'),
        "escape reached the terminal: {out:?}"
    );
    assert_eq!(out.lines().count(), 2, "a newline forged a line: {out:?}");
}

#[test]
fn answered_asks_show_only_on_asks_all() {
    let mut v = open(vec![ask(1, "q")]);
    v.answered.insert(id(7), id(8));
    let ui = ui("/none".into(), v);
    assert!(!run(&ui, "/asks").contains("answered"));
    assert!(run(&ui, "/asks all").contains("answered"));
}

#[test]
fn answer_sends_the_shown_text_for_the_numbered_ask_and_waits_on_the_log() {
    let s = signer(SignReply::Admitted {
        event: id(9),
        index: 4,
    });
    let ui = ui(
        s.path.clone(),
        open(vec![ask(1, "first"), ask(2, "second")]),
    );
    let out = run(&ui, "/answer 2 yes, do it");
    s.done.recv().unwrap();
    let seen = s.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!((seen[0].ask, seen[0].text.as_str()), (id(2), "yes, do it"));
    assert_eq!(seen[0].flock, id(200));
    assert!(
        out.contains("yes, do it"),
        "the draft is shown first: {out}"
    );
    assert!(
        out.contains("not yet in the verified log"),
        "the signer's word alone is not the log: {out}"
    );
}

#[test]
fn answer_says_in_log_only_when_the_verified_log_agrees() {
    let slot: Arc<Mutex<Option<Arc<FlockUi>>>> = Arc::default();
    let hook = Arc::clone(&slot);
    // while the human decides, the verified log learns of the closure
    let s = signer_with(
        SignReply::Admitted {
            event: id(9),
            index: 4,
        },
        move || {
            let mut v = AskView::default();
            v.answered.insert(id(1), id(9));
            hook.lock().unwrap().as_ref().unwrap().observe(v);
        },
    );
    let ui = Arc::new(ui(s.path.clone(), open(vec![ask(1, "q")])));
    *slot.lock().unwrap() = Some(Arc::clone(&ui));
    let out = run(&ui, "/answer 1 ok");
    s.done.recv().unwrap();
    assert!(out.contains("is in the verified log at index 4"), "{out}");
    assert!(!out.contains("not yet"), "{out}");
}

#[test]
fn the_draft_shown_before_sending_is_inert() {
    let s = signer(SignReply::Refused("n".into()));
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    let out = run(&ui, "/answer 1 pasted \u{1b}[2J text");
    s.done.recv().unwrap();
    assert!(!out.contains('\u{1b}'), "{out:?}");
}

#[test]
fn a_refusal_is_shown_inert_and_is_not_success() {
    let s = signer(SignReply::Refused("no\u{1b}[31m".into()));
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    let out = run(&ui, "/answer 1 text");
    s.done.recv().unwrap();
    assert!(out.contains("not signed"), "{out}");
    assert!(!out.contains('\u{1b}'), "{out:?}");
    assert!(!out.contains("verified log"), "{out}");
}

#[test]
fn no_signer_names_the_signer_command_and_does_not_retry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ui = ui(tmp.path().join("absent.sock"), open(vec![ask(1, "q")]));
    let (_, back) = run_split(&ui, "/answer 1 text");
    let out = back.join("\n");
    assert!(out.contains("amesh flock sign"), "{out}");
    assert!(out.contains("run /answer again"), "{out}");
    assert!(!out.contains("unknown"), "{out}");
    assert!(!out.contains("verified log"), "{out}");
}

#[test]
fn a_bad_number_or_empty_text_sends_nothing() {
    let s = signer(SignReply::Refused("unused".into()));
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    for line in [
        "/answer",
        "/answer 0 x",
        "/answer 2 x",
        "/answer one x",
        "/answer 1",
        "/answer 1   ",
    ] {
        assert!(run(&ui, line).starts_with("usage"), "{line}");
    }
    assert!(s.seen.lock().unwrap().is_empty());
}

#[test]
fn flock_origin_input_cannot_run_either_command() {
    let s = signer(SignReply::Admitted {
        event: id(9),
        index: 4,
    });
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let store = newt_core::ConversationStore::new(tmp.path().join("state"), &ws, 100).unwrap();
    let conv = newt_core::new_conversation_id();
    store.create_with_id(&conv, "t", None).unwrap();
    store
        .enqueue_flock_task(
            &conv,
            &newt_core::NewFlockTask {
                body: "b".into(),
                flock_cid: "f".into(),
                event_cid: "e".into(),
                evidence_cid: "c".into(),
                evidence: "{}".into(),
            },
        )
        .unwrap();
    let row = store.peek_flock_task(&conv).unwrap().unwrap();
    let origin = ModelInputOrigin::FlockTask {
        task: Box::new(row),
    };
    for line in ["/asks", "/asks all", "/answer 1 yes"] {
        assert_eq!(
            command(origin.is_operator(), line, Some(&ui), &mut |_| {}),
            None,
            "{line}"
        );
    }
    assert!(
        s.seen.lock().unwrap().is_empty(),
        "nothing reached the signer"
    );
}

#[test]
fn other_commands_and_a_missing_flock_are_not_swallowed() {
    let ui = ui("/none".into(), AskView::default());
    assert_eq!(command(true, "/status", Some(&ui), &mut |_| {}), None);
    assert_eq!(command(true, "/askswhat", Some(&ui), &mut |_| {}), None);
    let none = command(true, "/asks", None, &mut |_| {}).unwrap().join(" ");
    assert!(none.contains("no flock"), "{none}");
}

#[test]
fn the_draft_and_signer_instructions_are_shown_before_the_signer_can_reply() {
    let shown: Arc<Mutex<Vec<String>>> = Arc::default();
    let shown_at_reply: Arc<Mutex<Option<Vec<String>>>> = Arc::default();
    let (live, snap) = (Arc::clone(&shown), Arc::clone(&shown_at_reply));
    // the signer double replies only after taking a copy of what was displayed
    let s = signer_with(SignReply::Refused("n".into()), move || {
        *snap.lock().unwrap() = Some(live.lock().unwrap().clone());
    });
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    let sink = Arc::clone(&shown);
    let back = command(true, "/answer 1 go ahead", Some(&ui), &mut |l| {
        sink.lock().unwrap().push(l.to_owned());
    })
    .unwrap();
    s.done.recv().unwrap();
    let at_reply = shown_at_reply.lock().unwrap().clone().expect("signer ran");
    assert_eq!(at_reply.len(), 2, "{at_reply:?}");
    assert!(at_reply[0].contains("go ahead"), "{at_reply:?}");
    assert!(at_reply[1].contains("amesh flock sign"), "{at_reply:?}");
    assert!(
        back.iter().all(|l| !l.contains("go ahead")),
        "the draft is not held back for the end: {back:?}"
    );
}

#[test]
fn a_hangup_after_the_request_is_unknown_not_retry_advice() {
    let s = hangup_with(|| {});
    let ui = ui(s.path.clone(), open(vec![ask(1, "q")]));
    let out = run(&ui, "/answer 1 text");
    s.done.recv().unwrap();
    assert_eq!(s.seen.lock().unwrap().len(), 1, "the request was consumed");
    assert!(out.contains("outcome is unknown"), "{out}");
    assert!(out.contains("/asks"), "{out}");
    assert!(!out.contains("run /answer again"), "{out}");
    assert!(!out.contains("answered:"), "{out}");
}

#[test]
fn a_hangup_stays_unknown_even_once_the_view_shows_the_ask_answered() {
    let slot: Arc<Mutex<Option<Arc<FlockUi>>>> = Arc::default();
    let hook = Arc::clone(&slot);
    // the human signed and the follower learned of it, but the reply was lost
    let s = hangup_with(move || {
        let mut v = AskView::default();
        v.answered.insert(id(1), id(9));
        hook.lock().unwrap().as_ref().unwrap().observe(v);
    });
    let ui = Arc::new(ui(s.path.clone(), open(vec![ask(1, "q")])));
    *slot.lock().unwrap() = Some(Arc::clone(&ui));
    let out = run(&ui, "/answer 1 text");
    s.done.recv().unwrap();
    // no event came back to match, so the log cannot be said to confirm it
    assert!(out.contains("outcome is unknown"), "{out}");
    assert!(!out.contains("run /answer again"), "{out}");
    assert!(!out.contains("is in the verified log"), "{out}");
    assert!(
        run(&ui, "/asks all").contains("answered"),
        "the view shows it"
    );
}
