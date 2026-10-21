//! Replies against a real in-process flock server and real member sessions:
//! the Ack, Result and Verdict are signed by the session (newt holds no key),
//! count only once a fresh follower's verified log shows them, and a reply
//! with no clear answer is never sent twice.
#![cfg(unix)]

mod common;

use agent_mesh_flock::{Body, Kind, Outcome};
use agent_mesh_flock_server::Follower;
use agent_mesh_flock_session::{session_prover, Reader, RealFs};
use common::*;
use newt_flock::{
    open_tasks, present, Evidence, Local, Phase, Replies, Reply, Sent, Step, ToolCaller,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<serde_json::Value>>>;

struct World {
    r: Rig,
    shep: Enrolled,
    h1: RealSession,
    brief: Evidence,
    review: Evidence,
}

fn world() -> World {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = RealSession::new(&r, "h1");
    r.event(
        &mut shep,
        Kind::Brief,
        vec![],
        Body::Brief {
            to: "h1".into(),
            lane: "\u{1b}[31mlane".into(),
        },
    );
    r.event(
        &mut shep,
        Kind::Review,
        vec![],
        Body::Review {
            to: "h1".into(),
            lane: "rev".into(),
            head: "deadbeef01".into(),
        },
    );
    let board = r
        .serving
        .as_ref()
        .unwrap()
        .with_server(|s| s.board().clone());
    let mut tasks = open_tasks(&board, "h1").into_iter().map(|t| t.evidence);
    let brief = tasks.next().unwrap();
    let review = tasks.next().unwrap();
    World {
        r,
        shep,
        h1,
        brief,
        review,
    }
}

fn tool(w: &World) -> (SessionTool, Calls) {
    let calls: Calls = Arc::default();
    (
        SessionTool {
            sock: w.h1.path.clone(),
            calls: Arc::clone(&calls),
        },
        calls,
    )
}

/// What a freshly started follower with no saved state verifies.
fn fresh_follower(w: &World) -> Follower {
    let dir = tempfile::tempdir().unwrap();
    let mut reader = Reader::open(w.r.genesis(), dir.path(), RealFs).unwrap();
    reader.sync(&w.r.sock, &session_prover(&w.h1.path)).unwrap();
    reader.follower().clone()
}

fn journal(w: &World) -> std::path::PathBuf {
    w.r.tmp.path().join("replies")
}

fn replies(w: &World, dir: &Path) -> (Replies<SessionTool>, Calls) {
    let (t, calls) = tool(w);
    (Replies::open(dir, "h1", t).unwrap(), calls)
}

/// A journal directory only its owner can enter (the umask may be looser).
fn private_tmp() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn count(f: &Follower, kind: Kind) -> usize {
    f.board()
        .entries()
        .filter(|e| e.event.kind == kind && e.member.as_deref() == Some("h1"))
        .count()
}

#[test]
fn session_signed_ack_result_and_verdict_are_confirmed_by_a_fresh_follower() {
    let w = world();
    let dir = journal(&w);
    let (mut rp, calls) = replies(&w, &dir);

    let ack = Reply::ack(&w.brief);
    assert!(matches!(rp.send(&ack).unwrap(), Sent::Admitted { .. }));
    let res = Reply::result(&w.brief, true).unwrap();
    assert!(matches!(rp.send(&res).unwrap(), Sent::Admitted { .. }));
    let ver = Reply::verdict(&w.review, true).unwrap();
    assert!(matches!(rp.send(&ver).unwrap(), Sent::Admitted { .. }));
    assert_eq!(calls.lock().unwrap().len(), 3);

    // the admitted answer is not proof: only the verified log confirms
    for step in [Step::Ack, Step::Finish] {
        assert!(matches!(
            rp.state(&w.brief.flock, &w.brief.event, step),
            Some(Sent::Admitted { .. })
        ));
    }
    let fol = fresh_follower(&w);
    assert_eq!(rp.reconcile(&fol).unwrap(), 3);
    assert!(matches!(
        rp.state(&w.brief.flock, &w.brief.event, Step::Finish),
        Some(Sent::Confirmed { .. })
    ));
    // signed by the h1 session, with the right references, and the verdict on
    // the exact head the review cited
    assert_eq!(count(&fol, Kind::Ack), 1);
    assert_eq!(count(&fol, Kind::Result), 1);
    assert_eq!(count(&fol, Kind::Verdict), 1);
    let v = fol
        .board()
        .entries()
        .find(|e| e.event.kind == Kind::Verdict)
        .unwrap();
    assert!(matches!(
        &v.event.body,
        Body::Verdict { head, approved: true } if head == "deadbeef01"
    ));
    assert_eq!(
        v.event
            .refs
            .get("review")
            .map(ToString::to_string)
            .as_deref(),
        Some(w.review.event.as_str())
    );
    let r = fol
        .board()
        .entries()
        .find(|e| e.event.kind == Kind::Result)
        .unwrap();
    assert!(matches!(
        &r.event.body,
        Body::Result {
            outcome: Outcome::Completed
        }
    ));
    // newt never named a signer: the tool arguments carry no author or key
    for a in calls.lock().unwrap().iter() {
        let keys: Vec<_> = a.as_object().unwrap().keys().cloned().collect();
        assert!(keys
            .iter()
            .all(|k| matches!(k.as_str(), "kind" | "refs" | "body")));
    }
}

#[test]
fn a_reply_that_admission_refuses_is_shown_and_not_in_the_log() {
    let w = world();
    let (mut rp, calls) = replies(&w, &journal(&w));
    // an Ack of an event that does not exist
    let mut bogus = w.brief.clone();
    bogus.event = w.brief.flock.clone();
    let ack = Reply::ack(&bogus);
    let Sent::Refused { why } = rp.send(&ack).unwrap() else {
        panic!("a reference to nothing was not refused")
    };
    assert!(!why.is_empty());
    let fol = fresh_follower(&w);
    assert_eq!(count(&fol, Kind::Ack), 0);
    assert_eq!(rp.reconcile(&fol).unwrap(), 0);
    // shown, and a refusal is not retried by itself
    let v = present(&bogus, fol.board(), Local::Running, &rp);
    assert!(matches!(v.phase, Phase::Refused(_)), "{v:?}");
    assert!(v.line().contains("refused"));
    rp.send(&ack).unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1, "no retry");
}

#[test]
fn a_reply_with_no_clear_answer_is_not_sent_again_and_restart_settles_it_from_the_log() {
    let w = world();
    let dir = journal(&w);
    let (mut rp, calls) = replies(&w, &dir);
    // admitted, but the answer is lost
    *w.h1.fault.lock().unwrap() = Fault::DropAnswerAfterAdmit;
    let ack = Reply::ack(&w.brief);
    assert!(matches!(rp.send(&ack).unwrap(), Sent::Ambiguous { .. }));
    *w.h1.fault.lock().unwrap() = Fault::None;
    // asking again does not emit again
    assert!(matches!(rp.send(&ack).unwrap(), Sent::Ambiguous { .. }));
    assert_eq!(calls.lock().unwrap().len(), 1);
    let fol = fresh_follower(&w);
    assert_eq!(
        count(&fol, Kind::Ack),
        1,
        "exactly one ack, not a duplicate"
    );
    let v = present(&w.brief, fol.board(), Local::Running, &rp);
    assert_eq!(v.phase, Phase::Accepted, "ack not yet confirmed: {v:?}");

    // a restart: a new Replies on the same directory reads the journal and
    // confirms from the log without any call
    drop(rp);
    let (mut again, calls2) = replies(&w, &dir);
    assert!(matches!(
        again.state(&w.brief.flock, &w.brief.event, Step::Ack),
        Some(Sent::Ambiguous { .. })
    ));
    assert_eq!(again.reconcile(&fol).unwrap(), 1);
    assert!(matches!(
        again.state(&w.brief.flock, &w.brief.event, Step::Ack),
        Some(Sent::Confirmed { .. })
    ));
    assert!(calls2.lock().unwrap().is_empty());
    let v = present(&w.brief, fol.board(), Local::Running, &again);
    assert_eq!(v.phase, Phase::Running);
}

#[test]
fn a_lost_request_that_never_reached_the_session_stays_pending_and_is_not_blindly_resent() {
    let w = world();
    let (mut rp, calls) = replies(&w, &journal(&w));
    *w.h1.fault.lock().unwrap() = Fault::DropBeforeAdmit;
    let res = Reply::result(&w.brief, false).unwrap();
    assert!(matches!(rp.send(&res).unwrap(), Sent::Ambiguous { .. }));
    *w.h1.fault.lock().unwrap() = Fault::None;
    let fol = fresh_follower(&w);
    // the log has nothing: it stays pending, and still nothing is re-sent
    assert_eq!(rp.reconcile(&fol).unwrap(), 0);
    assert!(matches!(rp.send(&res).unwrap(), Sent::Ambiguous { .. }));
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(count(&fresh_follower(&w), Kind::Result), 0);
    let v = present(&w.brief, fol.board(), Local::Running, &rp);
    assert!(matches!(v.phase, Phase::ReplyPending(_)), "{v:?}");
}

#[test]
fn a_crash_between_the_intent_and_the_answer_leaves_an_intent_that_is_never_re_sent() {
    let w = world();
    let dir = journal(&w);
    // the process dies inside the call
    struct Dies;
    impl ToolCaller for Dies {
        fn call_tool(
            &mut self,
            _: &str,
            _: serde_json::Value,
        ) -> Result<newt_flock::ToolReply, newt_flock::Lost> {
            panic!("killed mid-call");
        }
    }
    let ack = Reply::ack(&w.brief);
    let mut dying = Replies::open(&dir, "h1", Dies).unwrap();
    let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dying.send(&ack)));
    assert!(died.is_err());
    drop(dying);

    let (mut rp, calls) = replies(&w, &dir);
    assert_eq!(
        rp.state(&w.brief.flock, &w.brief.event, Step::Ack),
        Some(&Sent::Intent),
        "the intent was durable before the call"
    );
    assert_eq!(rp.send(&ack).unwrap(), Sent::Intent);
    assert!(calls.lock().unwrap().is_empty(), "no blind re-emit");
    let v = present(&w.brief, fresh_follower(&w).board(), Local::Running, &rp);
    assert_eq!(v.phase, Phase::Accepted);

    // had the request reached the session before the crash, the log shows it
    let (mut other, _) = replies(&w, &w.r.tmp.path().join("other-journal"));
    other.send(&ack).unwrap();
    assert_eq!(rp.reconcile(&fresh_follower(&w)).unwrap(), 1);
    assert!(matches!(
        rp.state(&w.brief.flock, &w.brief.event, Step::Ack),
        Some(Sent::Confirmed { .. })
    ));
}

#[test]
fn a_torn_last_journal_line_is_dropped_and_the_next_write_is_clean() {
    let w = world();
    let dir = journal(&w);
    let (mut rp, _) = replies(&w, &dir);
    let ack = Reply::ack(&w.brief);
    rp.send(&ack).unwrap();
    drop(rp);
    // a crash mid-append of the next line
    let path = dir.join("replies.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"reply\":{\"flock\":\"x\"");
    std::fs::write(&path, text).unwrap();

    let (mut rp, _) = replies(&w, &dir);
    assert!(matches!(
        rp.state(&w.brief.flock, &w.brief.event, Step::Ack),
        Some(Sent::Admitted { .. })
    ));
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    drop(rp);
    let (rp, _) = replies(&w, &dir);
    assert!(rp
        .state(&w.brief.flock, &w.brief.event, Step::Finish)
        .is_some());
}

#[test]
fn a_reply_is_built_only_from_the_evidence_and_a_verdict_binds_the_exact_head() {
    let w = world();
    assert!(
        Reply::result(&w.review, true).is_err(),
        "a result answers a brief"
    );
    assert!(
        Reply::verdict(&w.brief, true).is_err(),
        "a verdict answers a review"
    );
    let v = Reply::verdict(&w.review, false).unwrap();
    assert_eq!(v.head.as_deref(), Some("deadbeef01"));
    assert_eq!(v.body["verdict"]["head"], "deadbeef01");
    assert_eq!(v.body["verdict"]["approved"], false);
    assert_eq!(v.kind, Kind::Verdict);
    assert_eq!(v.role, "review");
    let a = Reply::ack(&w.brief);
    assert_eq!((a.kind, a.role.as_str()), (Kind::Ack, "of"));
    assert_ne!(a.key(), Reply::result(&w.brief, true).unwrap().key());
}

#[test]
fn the_row_names_flock_member_index_lane_head_state_and_the_signers_role_inertly() {
    let w = world();
    let (mut rp, _) = replies(&w, &journal(&w));
    let fol = fresh_follower(&w);
    let q = present(&w.brief, fol.board(), Local::Queued, &rp);
    assert_eq!(q.phase, Phase::Queued);
    assert_eq!(q.from_role.as_deref(), Some("shepherd"));
    let line = q.line();
    assert!(
        line.contains("as h1") && line.contains("from shep (shepherd)"),
        "{line}"
    );
    assert!(line.contains(&format!("#{}", w.brief.index)), "{line}");
    assert!(!line.contains('\u{1b}'), "lane text is inert: {line:?}");
    let r = present(&w.review, fol.board(), Local::Queued, &rp);
    assert!(r.line().contains("head deadbeef01"), "{}", r.line());

    rp.send(&Reply::ack(&w.brief)).unwrap();
    rp.reconcile(&fresh_follower(&w)).unwrap();
    let fol = fresh_follower(&w);
    assert_eq!(
        present(&w.brief, fol.board(), Local::Running, &rp).phase,
        Phase::Running
    );
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    assert!(matches!(
        present(&w.brief, fol.board(), Local::Running, &rp).phase,
        Phase::ReplyPending(_)
    ));
    rp.reconcile(&fresh_follower(&w)).unwrap();
    assert_eq!(
        present(&w.brief, fol.board(), Local::Running, &rp).phase,
        Phase::Replied
    );
    let _ = &w.shep;
}

#[test]
fn an_mcp_tool_result_is_read_as_text_or_error_and_anything_else_is_no_answer() {
    use newt_flock::{Lost, ToolReply};
    let ok = serde_json::json!({"content": [{"type": "text", "text": "admitted x at 3"}]});
    assert_eq!(
        ToolReply::from_mcp(&ok),
        Ok(ToolReply::Text("admitted x at 3".into()))
    );
    let err =
        serde_json::json!({"isError": true, "content": [{"type": "text", "text": "refused: no"}]});
    assert_eq!(
        ToolReply::from_mcp(&err),
        Ok(ToolReply::Error("refused: no".into()))
    );
    assert!(matches!(
        ToolReply::from_mcp(&serde_json::json!({"content": []})),
        Err(Lost(_))
    ));
    // an "admitted" text that does not parse is not taken as admitted
    let (mut rp, _) = {
        let dir = private_tmp();
        (
            Replies::open(
                &dir,
                "h1",
                newt_flock::FnCaller(|_: &str, _| Ok(ToolReply::Text("admitted soon".into()))),
            )
            .unwrap(),
            (),
        )
    };
    let ev = world().brief;
    assert!(matches!(
        rp.send(&Reply::ack(&ev)).unwrap(),
        Sent::Ambiguous { .. }
    ));
}

/// A pending reply to one task is not confirmed by the same member's reply of
/// the same kind to another task. (A verdict on another head cannot reach the
/// log: admission refuses it with HeadMismatch, so that check is a backstop.)
#[test]
fn a_log_entry_for_another_task_does_not_confirm_a_pending_reply() {
    use newt_flock::{FnCaller, ToolReply};
    let w = world();
    let dir = journal(&w);
    {
        // the decoys go out through another journal: only the log is shared
        let (mut rp, _) = replies(&w, &private_tmp());
        rp.send(&Reply::ack(&w.brief)).unwrap();
    }
    // the real replies are pending with no clear answer
    let mut rp = Replies::open(
        &dir,
        "h1",
        FnCaller(|_: &str, _| Ok(ToolReply::Text("???".into()))),
    )
    .unwrap();
    let ack_review = Reply::ack(&w.review);
    let verdict = Reply::verdict(&w.review, true).unwrap();
    assert!(matches!(
        rp.send(&ack_review).unwrap(),
        Sent::Ambiguous { .. }
    ));
    assert!(matches!(rp.send(&verdict).unwrap(), Sent::Ambiguous { .. }));
    rp.reconcile(&fresh_follower(&w)).unwrap();
    for r in [&ack_review, &verdict] {
        assert!(
            !matches!(
                rp.state(&r.flock, &r.event, r.step),
                Some(Sent::Confirmed { .. })
            ),
            "{:?} must stay pending",
            r.kind
        );
    }
}

/// `admitted` text whose index is not a number is no clear answer, and the
/// index a clear answer carries is the one the log shows.
#[test]
fn an_admitted_answer_carries_the_log_index_and_a_bad_index_is_no_answer() {
    use newt_flock::{FnCaller, ToolReply};
    let w = world();
    let (mut rp, _) = replies(&w, &journal(&w));
    let Sent::Admitted { index, .. } = rp.send(&Reply::ack(&w.brief)).unwrap() else {
        panic!("admitted")
    };
    let fol = fresh_follower(&w);
    let at = fol
        .board()
        .entries()
        .find(|e| e.event.kind == Kind::Ack)
        .unwrap()
        .index;
    assert_eq!(index, at);

    let dir = private_tmp();
    let cid = fol
        .board()
        .entries()
        .find(|e| e.event.kind == Kind::Ack)
        .unwrap()
        .id
        .to_string();
    let mut bad = Replies::open(
        &dir,
        "h1",
        FnCaller(move |_: &str, _| Ok(ToolReply::Text(format!("admitted {cid} at soon")))),
    )
    .unwrap();
    assert!(matches!(
        bad.send(&Reply::ack(&w.brief)).unwrap(),
        Sent::Ambiguous { .. }
    ));
}

// ---- Codex round 1 on #2977 ------------------------------------------------

fn unclear() -> newt_flock::FnCaller<
    impl FnMut(&str, serde_json::Value) -> Result<newt_flock::ToolReply, newt_flock::Lost>,
> {
    newt_flock::FnCaller(|_: &str, _| Ok(newt_flock::ToolReply::Text("???".into())))
}

/// Finding 1: one owner at a time. A second opener on the same directory is
/// refused (it would see no intent and emit the same reply again); once the
/// first is gone the journal opens again with its record.
#[test]
fn a_second_journal_owner_is_refused_until_the_first_is_gone() {
    let w = world();
    let dir = journal(&w);
    let (mut first, calls) = replies(&w, &dir);
    let second = replies_try(&w, &dir);
    let why = second.err().expect("two owners on one journal");
    assert!(why.contains("another"), "{why}");
    first.send(&Reply::ack(&w.brief)).unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    drop(first);
    let (again, _) = replies(&w, &dir);
    assert!(again
        .state(&w.brief.flock, &w.brief.event, Step::Ack)
        .is_some());
}

fn replies_try(w: &World, dir: &Path) -> Result<Replies<SessionTool>, String> {
    let (t, _) = tool(w);
    Replies::open(dir, "h1", t)
}

/// Finding 2: a complete row whose newline was not written is repaired, so the
/// next append does not glue onto it and every row reads after a reopen.
#[test]
fn a_row_without_its_newline_is_terminated_before_the_next_append() {
    let w = world();
    let dir = journal(&w);
    let (mut rp, _) = replies(&w, &dir);
    rp.send(&Reply::ack(&w.brief)).unwrap();
    drop(rp);
    let path = dir.join("replies.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.trim_end_matches('\n')).unwrap();

    let (mut rp, _) = replies(&w, &dir);
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    drop(rp);
    let (rp, _) = replies(&w, &dir);
    for step in [Step::Ack, Step::Finish] {
        assert!(
            rp.state(&w.brief.flock, &w.brief.event, step).is_some(),
            "{step:?} lost"
        );
    }
}

/// Finding 3: the log's reply must be the one intended. The opposite verdict
/// or result does not confirm; the journal says the log disagrees, and the
/// intended reply is never sent.
#[test]
fn an_opposite_verdict_or_result_in_the_log_does_not_confirm_the_intended_reply() {
    let w = world();
    // the log gets a rejecting verdict and a failed result, from this member
    let (mut via, _) = replies(&w, &private_tmp());
    via.send(&Reply::verdict(&w.review, false).unwrap())
        .unwrap();
    via.send(&Reply::result(&w.brief, false).unwrap()).unwrap();
    drop(via);

    // this journal intended to approve and to complete, and lost both answers
    let dir = journal(&w);
    let mut rp = Replies::open(&dir, "h1", unclear()).unwrap();
    let approve = Reply::verdict(&w.review, true).unwrap();
    let complete = Reply::result(&w.brief, true).unwrap();
    rp.send(&approve).unwrap();
    rp.send(&complete).unwrap();
    assert_eq!(rp.reconcile(&fresh_follower(&w)).unwrap(), 0);
    for r in [&approve, &complete] {
        let s = rp.state(&r.flock, &r.event, r.step).unwrap();
        assert!(
            matches!(s, Sent::Contradicted { .. }),
            "{:?} -> {s:?}",
            r.kind
        );
    }
    // shown as a refusal-like end state, never as replied
    let row = present(&w.review, fresh_follower(&w).board(), Local::Running, &rp);
    assert!(matches!(row.phase, Phase::Refused(_)), "{:?}", row.phase);
    // and a journal key never stands in for a different requested body
    let opposite = Reply::verdict(&w.review, false).unwrap();
    assert!(rp.send(&opposite).is_err());
}

/// Finding 4: the journal's directory entry is durable before the first
/// emission, and a failing sync stops the open.
#[test]
fn the_journal_directory_is_synced_before_any_emission() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let w = world();
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let o = Arc::clone(&order);
    let caller = newt_flock::FnCaller(move |_: &str, _| {
        o.lock().unwrap().push("emit");
        Ok(newt_flock::ToolReply::Text("???".into()))
    });
    let o = Arc::clone(&order);
    let syncs = Arc::new(AtomicUsize::new(0));
    let s2 = Arc::clone(&syncs);
    let mut rp = Replies::open_with(
        &journal(&w),
        "h1",
        caller,
        Box::new(move |_dir: &Path| {
            o.lock().unwrap().push("sync");
            s2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    )
    .unwrap();
    rp.send(&Reply::ack(&w.brief)).unwrap();
    let seen = order.lock().unwrap().clone();
    assert_eq!(seen.first(), Some(&"sync"), "{seen:?}");
    assert_eq!(seen.iter().position(|x| *x == "emit"), Some(seen.len() - 1));
    assert!(syncs.load(Ordering::SeqCst) >= 1);

    let failing = Replies::open_with(
        &w.r.tmp.path().join("other-journal"),
        "h1",
        unclear(),
        Box::new(|_: &Path| Err(std::io::Error::other("disk"))),
    );
    assert!(failing.is_err(), "an unsynced journal must not open");
}

/// Finding 5: the journal lives in a private directory (upstream's check), and
/// the journal file itself is not a link.
#[test]
fn the_journal_directory_must_be_private_and_the_file_not_a_link() {
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    let w = world();
    let tmp = w.r.tmp.path();
    // world-readable
    let open_dir = tmp.join("open");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(replies_try(&w, &open_dir).is_err(), "0755 accepted");
    // a link to a real private directory
    let real = tmp.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = tmp.join("link");
    symlink(&real, &link).unwrap();
    assert!(
        replies_try(&w, &link).is_err(),
        "a linked directory accepted"
    );
    assert!(
        real.read_dir().unwrap().next().is_none(),
        "wrote through it"
    );
    // a private directory whose journal is a link elsewhere
    let priv_dir = tmp.join("priv");
    std::fs::create_dir(&priv_dir).unwrap();
    std::fs::set_permissions(&priv_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let elsewhere = tmp.join("elsewhere.jsonl");
    std::fs::write(&elsewhere, "").unwrap();
    symlink(&elsewhere, priv_dir.join("replies.jsonl")).unwrap();
    assert!(
        replies_try(&w, &priv_dir).is_err(),
        "a linked journal accepted"
    );
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "");
    // and the ordinary case still works
    let ok = tmp.join("fine");
    assert!(replies_try(&w, &ok).is_ok());
}

// ---- Codex round 2 on #2977 --------------------------------------------------

/// Finding 4 (r4): existence is not consulted. Every open syncs the journal
/// directory and each ancestor up to `/`, before any emission, whoever
/// created which directory.
#[test]
fn every_open_syncs_the_whole_chain_before_any_emission() {
    let w = world();
    let base = w.r.tmp.path().join("base");
    // "another opener" already made the first level
    std::fs::create_dir_all(base.join("a")).unwrap();
    let dir = base.join("a").join("b");
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (l, l2) = (Arc::clone(&log), Arc::clone(&log));
    let caller = newt_flock::FnCaller(move |_: &str, _| {
        l2.lock().unwrap().push("emit".into());
        Ok(newt_flock::ToolReply::Text("???".into()))
    });
    let mut rp = Replies::open_with(
        &dir,
        "h1",
        caller,
        Box::new(move |p: &Path| {
            l.lock().unwrap().push(p.display().to_string());
            Ok(())
        }),
    )
    .unwrap();
    rp.send(&Reply::ack(&w.brief)).unwrap();
    let want: Vec<String> = dir
        .ancestors()
        .map(|p| p.display().to_string())
        .chain(["emit".to_string()])
        .collect();
    assert_eq!(*log.lock().unwrap(), want, "child first, up to the root");
}

/// A sync that fails stops the open (nothing is emitted); the retry syncs the
/// whole chain again, though every directory now exists.
#[test]
fn a_failed_sync_fails_the_open_and_the_retry_syncs_everything_again() {
    let w = world();
    let dir = w.r.tmp.path().join("x").join("y");
    let first = Arc::new(Mutex::new(true));
    let f = Arc::clone(&first);
    let failed = Replies::open_with(
        &dir,
        "h1",
        unclear(),
        Box::new(move |_: &Path| {
            if std::mem::replace(&mut *f.lock().unwrap(), false) {
                Err(std::io::Error::other("disk"))
            } else {
                Ok(())
            }
        }),
    );
    assert!(failed.is_err());
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let l = Arc::clone(&log);
    let rp = Replies::open_with(
        &dir,
        "h1",
        unclear(),
        Box::new(move |p: &Path| {
            l.lock().unwrap().push(p.display().to_string());
            Ok(())
        }),
    );
    assert!(rp.is_ok(), "the failed open left the journal locked");
    let want: Vec<String> = dir.ancestors().map(|p| p.display().to_string()).collect();
    assert_eq!(*log.lock().unwrap(), want);
}

/// Finding 6: a multibyte character cut in half by a crash in the last row is
/// a torn tail (dropped), not an unreadable journal.
#[test]
fn a_multibyte_character_cut_mid_sequence_is_a_torn_tail() {
    let w = world();
    let dir = journal(&w);
    let (mut rp, _) = replies(&w, &dir);
    rp.send(&Reply::ack(&w.brief)).unwrap();
    drop(rp);
    let path = dir.join("replies.jsonl");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"reply\":{\"flock\":\"\xc3"); // half of "é"
    std::fs::write(&path, bytes).unwrap();

    let (mut rp, _) = replies(&w, &dir);
    assert!(rp
        .state(&w.brief.flock, &w.brief.event, Step::Ack)
        .is_some());
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    drop(rp);
    let (rp, _) = replies(&w, &dir);
    for step in [Step::Ack, Step::Finish] {
        assert!(rp.state(&w.brief.flock, &w.brief.event, step).is_some());
    }
}

/// Finding 7: a one-component relative directory has the empty path as its
/// parent, which cannot be synced, so the plan never names it. (Pure, so no
/// test changes the process's directory; `open` adds the resolved chain.)
#[test]
fn a_relative_dir_never_syncs_the_empty_path() {
    assert_eq!(
        newt_flock::sync_plan(Path::new("replies")),
        [Path::new("replies").to_path_buf()]
    );
    assert_eq!(
        newt_flock::sync_plan(Path::new("a/b")),
        [Path::new("a/b").to_path_buf(), Path::new("a").to_path_buf()]
    );
}

// ---- Codex round 4 on #2977 --------------------------------------------------

/// Finding 4 (r5): a symlinked ancestor. The lexical chain names the alias, but
/// the directory entries that must be durable are in the physical parents, so
/// the resolved chain is synced too, before any emission.
#[test]
fn a_symlinked_ancestor_has_its_physical_chain_synced_before_any_emission() {
    use std::os::unix::fs::symlink;
    let w = world();
    let base = std::fs::canonicalize(w.r.tmp.path()).unwrap().join("base");
    let physical = base.join("physical");
    let parent = physical.join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    symlink(&parent, base.join("alias")).unwrap();
    let dir = base.join("alias").join("journal");

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (l, l2) = (Arc::clone(&log), Arc::clone(&log));
    let caller = newt_flock::FnCaller(move |_: &str, _| {
        l2.lock().unwrap().push("emit".into());
        Ok(newt_flock::ToolReply::Text("???".into()))
    });
    let mut rp = Replies::open_with(
        &dir,
        "h1",
        caller,
        Box::new(move |p: &Path| {
            l.lock().unwrap().push(p.display().to_string());
            Ok(())
        }),
    )
    .unwrap();
    rp.send(&Reply::ack(&w.brief)).unwrap();
    let seen = log.lock().unwrap().clone();
    let at = |p: &Path| seen.iter().position(|x| *x == p.display().to_string());
    let emit = seen.iter().position(|x| x == "emit").unwrap();
    for p in [
        parent.join("journal"),
        parent.clone(),
        physical.clone(),
        base.clone(),
    ] {
        assert!(
            at(&p).is_some_and(|i| i < emit),
            "{} not synced before emission: {seen:?}",
            p.display()
        );
    }
    assert!(at(&dir).is_some(), "the lexical chain is kept: {seen:?}");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        seen.len(),
        "a directory synced twice: {seen:?}"
    );
}

// ---- the journal is a chain (content-addressable ratchet) -----------------

/// Three rows in a fresh journal (an Ack, a Result, a Verdict), and the lines
/// as written.
fn three_rows(w: &World, dir: &Path) -> Vec<String> {
    let (mut rp, _) = replies(w, dir);
    rp.send(&Reply::ack(&w.brief)).unwrap();
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    rp.send(&Reply::verdict(&w.review, true).unwrap()).unwrap();
    drop(rp);
    let text = std::fs::read_to_string(dir.join("replies.jsonl")).unwrap();
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    // each reply is an intent then its answer: the chain is longer than three
    assert!(lines.len() >= 6, "{} rows", lines.len());
    lines
}

fn write_lines(dir: &Path, lines: &[String]) {
    std::fs::write(dir.join("replies.jsonl"), lines.join("\n") + "\n").unwrap();
}

fn refused_open(w: &World, dir: &Path) -> String {
    let (t, calls) = tool(w);
    let before = calls.lock().unwrap().len();
    let err = Replies::open(dir, "h1", t)
        .err()
        .expect("a broken chain opened");
    assert_eq!(calls.lock().unwrap().len(), before, "nothing was emitted");
    err
}

#[test]
fn a_deleted_middle_row_fails_the_open() {
    let w = world();
    let dir = journal(&w);
    let mut lines = three_rows(&w, &dir);
    lines.remove(2);
    write_lines(&dir, &lines);
    assert!(refused_open(&w, &dir).contains("chain"), "clear error");
}

#[test]
fn two_swapped_rows_fail_the_open() {
    let w = world();
    let dir = journal(&w);
    let mut lines = three_rows(&w, &dir);
    lines.swap(1, 2);
    write_lines(&dir, &lines);
    assert!(refused_open(&w, &dir).contains("chain"));
}

#[test]
fn an_edited_row_fails_the_open() {
    let w = world();
    let dir = journal(&w);
    let mut lines = three_rows(&w, &dir);
    let edited = lines[3].replacen("completed", "failed", 1);
    assert_ne!(edited, lines[3], "the fixture must change something");
    lines[3] = edited;
    write_lines(&dir, &lines);
    assert!(refused_open(&w, &dir).contains("chain"));
}

/// A complete last row that breaks the chain is tampering, not a torn tail:
/// it is never silently dropped.
#[test]
fn a_complete_final_row_that_breaks_the_chain_is_not_dropped_as_torn() {
    let w = world();
    let dir = journal(&w);
    let mut lines = three_rows(&w, &dir);
    let last = lines.pop().unwrap();
    lines.remove(1); // the last row's parent is gone, too
    let _ = last.len();
    lines.push(last);
    // and the final newline is missing, as a crash could leave it
    std::fs::write(dir.join("replies.jsonl"), lines.join("\n")).unwrap();
    assert!(refused_open(&w, &dir).contains("chain"));
}

#[test]
fn an_intact_chain_reopens_and_extends() {
    let w = world();
    let dir = journal(&w);
    let lines = three_rows(&w, &dir);
    let (mut rp, _) = replies(&w, &dir);
    assert!(rp
        .state(&w.review.flock, &w.review.event, Step::Finish)
        .is_some());
    rp.send(&Reply::ack(&w.review)).unwrap();
    drop(rp);
    let after = std::fs::read_to_string(dir.join("replies.jsonl")).unwrap();
    assert!(after.lines().count() > lines.len());
    let (_rp, _) = replies(&w, &dir); // and the longer chain verifies
}

// ---- Codex round 6 on #2977 --------------------------------------------------

/// Finding 8: a failed append poisons the journal. The head is not advanced, a
/// retry on the same object writes and emits nothing, and a reopen (verify +
/// tail recovery) is the only way back, with the chain intact.
#[test]
fn a_failed_append_poisons_the_journal_until_it_is_reopened() {
    use std::io::Write as _;
    let w = world();
    let dir = journal(&w);
    let emitted = Arc::new(Mutex::new(0usize));
    let e = Arc::clone(&emitted);
    let caller = newt_flock::FnCaller(move |_: &str, _| {
        *e.lock().unwrap() += 1;
        Ok(newt_flock::ToolReply::Text("???".into()))
    });
    let one_shot = Arc::new(Mutex::new(true));
    let fault = Arc::clone(&one_shot);
    let mut rp = Replies::open_with_append(
        &dir,
        "h1",
        caller,
        Box::new(|_: &Path| Ok(())),
        // the line reaches the file, then the fsync fails: the file and the
        // in-memory head now disagree unless the journal refuses to go on
        Box::new(move |f: &mut std::fs::File, bytes: &[u8]| {
            f.write_all(bytes)?;
            if std::mem::replace(&mut *fault.lock().unwrap(), false) {
                return Err(std::io::Error::other("fsync"));
            }
            f.sync_all()
        }),
    )
    .unwrap();
    let ack = Reply::ack(&w.brief);
    assert!(rp.send(&ack).is_err(), "the failed append is an error");
    assert_eq!(
        *emitted.lock().unwrap(),
        0,
        "nothing left before the intent"
    );
    let before = std::fs::read(dir.join("replies.jsonl")).unwrap();
    // the fault is spent, yet the same object refuses to go on
    let again = rp.send(&Reply::result(&w.brief, true).unwrap());
    assert!(
        again.err().is_some_and(|e| e.contains("reopen")),
        "poisoned"
    );
    assert_eq!(*emitted.lock().unwrap(), 0);
    assert_eq!(std::fs::read(dir.join("replies.jsonl")).unwrap(), before);
    drop(rp);

    // reopening verifies the file (the intent line did land) and works again
    let (mut rp, _) = replies(&w, &dir);
    rp.send(&Reply::result(&w.brief, true).unwrap()).unwrap();
    drop(rp);
    let text = std::fs::read_to_string(dir.join("replies.jsonl")).unwrap();
    let geneses = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["node"]["parents"]
                .as_array()
                .is_some_and(Vec::is_empty)
        })
        .count();
    assert_eq!(geneses, 1, "a second genesis forked the chain");
    let (_rp, _) = replies(&w, &dir); // the whole chain still verifies
}
