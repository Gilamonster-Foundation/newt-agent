//! Intake against a real in-process flock server: which events become tasks,
//! that a restart or replay queues nothing twice, that a saved prefix is never
//! published before this run catches up, and that the evidence binds a
//! review's exact head.
#![cfg(unix)]

mod common;

use agent_mesh_flock::{Body, Kind, Outcome};
use common::*;
use content_addressable::ContentAddressable as _;
use newt_flock::{open_tasks, verify, Evidence, Inbox, Intake, NewTask, Sink, TaskKind};
use std::collections::BTreeSet;
use std::sync::{mpsc, Arc, Mutex};

/// A queue that remembers (flock, event) like the store does.
#[derive(Default, Clone)]
struct Queue {
    seen: Arc<Mutex<BTreeSet<(String, String)>>>,
    tasks: Arc<Mutex<Vec<NewTask>>>,
    /// Every call, new or not.
    calls: Arc<Mutex<usize>>,
}

impl Sink for Queue {
    fn enqueue(&mut self, task: &NewTask) -> Result<bool, String> {
        *self.calls.lock().unwrap() += 1;
        let key = (task.flock_cid.clone(), task.event_cid.clone());
        if !self.seen.lock().unwrap().insert(key) {
            return Ok(false);
        }
        self.tasks.lock().unwrap().push(task.clone());
        Ok(true)
    }
}

fn to_h1(r: &mut Rig, shep: &mut Enrolled, lane: &str) {
    r.event(
        shep,
        Kind::Brief,
        vec![],
        Body::Brief {
            to: "h1".into(),
            lane: lane.into(),
        },
    );
}

#[test]
fn only_open_briefs_and_reviews_addressed_to_the_member_become_tasks() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let _h1 = r.enrol("h1");
    to_h1(&mut r, &mut shep, "mine");
    r.event(
        &mut shep,
        Kind::Brief,
        vec![],
        Body::Brief {
            to: "shep".into(),
            lane: "not-mine".into(),
        },
    );
    r.event(
        &mut shep,
        Kind::Review,
        vec![],
        Body::Review {
            to: "h1".into(),
            lane: "mine".into(),
            head: "abc123".into(),
        },
    );
    let board = r
        .serving
        .as_ref()
        .unwrap()
        .with_server(|s| s.board().clone());
    let tasks = open_tasks(&board, "h1");
    let kinds: Vec<_> = tasks
        .iter()
        .map(|t| (t.evidence.kind, t.evidence.lane.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (TaskKind::Brief, "mine".to_owned()),
            (TaskKind::Review, "mine".to_owned())
        ],
        "in admission order, and nothing addressed to someone else"
    );
    assert!(open_tasks(&board, "shep")
        .iter()
        .all(|t| t.evidence.lane == "not-mine"));
    assert!(open_tasks(&board, "nobody").is_empty());
}

#[test]
fn a_brief_with_a_result_is_no_longer_open() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let mut h1 = r.enrol("h1");
    to_h1(&mut r, &mut shep, "done");
    let brief = r
        .serving
        .as_ref()
        .unwrap()
        .with_server(|s| s.board().entries().last().unwrap().id);
    r.event(&mut h1, Kind::Ack, vec![("of".into(), brief)], Body::Ack);
    let board = |r: &Rig| {
        r.serving
            .as_ref()
            .unwrap()
            .with_server(|s| s.board().clone())
    };
    assert_eq!(open_tasks(&board(&r), "h1").len(), 1, "acked is still open");
    r.event(
        &mut h1,
        Kind::Result,
        vec![("brief".into(), brief)],
        Body::Result {
            outcome: Outcome::Completed,
        },
    );
    assert!(open_tasks(&board(&r), "h1").is_empty());
}

#[test]
fn running_intake_again_over_the_same_board_queues_nothing_twice() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    to_h1(&mut r, &mut shep, "a");
    let s = session_for(&r, "h1", &h1);
    let queue = Queue::default();
    let flock = r.genesis().content_id().unwrap();
    let run = |queue: &Queue| {
        let (tx, rx) = mpsc::channel();
        let (mut intake, mut sink) = (Intake::new("h1", flock), queue.clone());
        let inbox = Inbox::start_with(config(&r, &s), move |f| {
            let _ = tx.send(intake.reconcile(f, &mut sink));
        })
        .unwrap();
        until_caught(&inbox);
        drop(inbox);
        rx.try_iter().sum::<usize>()
    };
    assert_eq!(run(&queue), 1);
    assert_eq!(
        *queue.calls.lock().unwrap(),
        1,
        "the startup state and the catch-up state are the same board: it is offered once"
    );
    // a restart: the same saved state, the same queue
    assert_eq!(run(&queue), 0, "a restart queued the task again");
    // a replay from nothing (the reader's state is lost) into the same queue
    std::fs::remove_dir_all(r.tmp.path().join("newt-state")).unwrap();
    assert_eq!(run(&queue), 0, "a replay queued the task again");
    assert_eq!(queue.tasks.lock().unwrap().len(), 1);
}

/// Review 2976 r2 finding 2: a saved board is only a verified prefix (the
/// reader marks no catch-up), so it is never published on its own.
#[test]
fn a_saved_board_is_not_published_before_this_run_catches_up() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    to_h1(&mut r, &mut shep, "early");
    let s = session_for(&r, "h1", &h1);
    let flock = r.genesis().content_id().unwrap();
    // first life: the reader saves the board, the sink is lost before it queues
    let inbox = Inbox::start(config(&r, &s)).unwrap();
    until_caught(&inbox);
    drop(inbox);
    // second life, with the server gone: this run never catches up
    r.serving.take().unwrap().stop();
    let queue = Queue::default();
    let (mut intake, mut sink) = (Intake::new("h1", flock), queue.clone());
    let inbox = Inbox::start_with(config(&r, &s), move |f| {
        intake.reconcile(f, &mut sink);
    })
    .unwrap();
    // the worker has tried a pass and found the server away
    loop {
        match inbox.events().recv().unwrap() {
            newt_flock::Observed::Unavailable(_) => break,
            newt_flock::Observed::Caught(_) => panic!("caught up with no server"),
            _ => {}
        }
    }
    drop(inbox);
    assert!(
        queue.tasks.lock().unwrap().is_empty(),
        "a saved prefix was published without a catch-up"
    );
}

#[test]
fn a_reviews_exact_head_is_in_its_evidence_and_text_and_tampering_is_caught() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let _h1 = r.enrol("h1");
    r.event(
        &mut shep,
        Kind::Review,
        vec![],
        Body::Review {
            to: "h1".into(),
            lane: "l".into(),
            head: "0123abcd".into(),
        },
    );
    let board = r
        .serving
        .as_ref()
        .unwrap()
        .with_server(|s| s.board().clone());
    let task = open_tasks(&board, "h1").remove(0);
    assert_eq!(task.evidence.head.as_deref(), Some("0123abcd"));
    let at = board
        .entries()
        .find(|e| e.event.kind == Kind::Review)
        .unwrap();
    assert_eq!(
        task.evidence.index, at.index,
        "the verified admission index"
    );
    assert_eq!(task.evidence.event, at.id.to_string());
    assert_eq!(task.evidence.from, "shep");
    let new = task.to_new().unwrap();
    assert!(
        new.body.contains("head under review: 0123abcd"),
        "{}",
        new.body
    );
    let ev: Evidence = verify(&new).unwrap();
    assert_eq!(ev, task.evidence);

    // a head edited in the stored evidence no longer matches its content id
    let mut t = new.clone();
    t.evidence = t.evidence.replace("0123abcd", "ffffffff");
    assert!(verify(&t).unwrap_err().contains("content id"));
    // the same evidence in another spelling (whitespace) is not the canonical
    // form the content id was made from
    let mut t = new.clone();
    t.evidence = t.evidence.replace(",\"", ", \"");
    assert!(verify(&t).unwrap_err().contains("canonical"));
    // a different head in the text than the evidence renders to
    let mut t = new.clone();
    t.body = t.body.replace("0123abcd", "ffffffff");
    assert!(verify(&t).unwrap_err().contains("renders"));
    // an evidence whose event is not the key it is queued under
    let mut t = new.clone();
    t.event_cid = "other".into();
    assert!(verify(&t).unwrap_err().contains("queue key"));
    // evidence and its id both rewritten together still fails the key and render checks
    let mut other = task.evidence.clone();
    other.head = Some("ffffffff".into());
    let mut t = new;
    t.evidence = serde_json::to_string(&other).unwrap();
    t.evidence_cid = other.cid().unwrap().to_string();
    assert!(verify(&t).is_err(), "text still says the old head");
}

#[test]
fn text_from_the_log_is_rendered_inert() {
    let ev = Evidence {
        flock: "f".into(),
        event: "e".into(),
        index: 7,
        kind: TaskKind::Brief,
        from: "sh\u{1b}[31mep".into(),
        to: "h1".into(),
        lane: "l\u{7}\r\nIGNORE ALL PREVIOUS".into(),
        head: Some("h\u{9b}2J".into()),
        files: vec![],
    };
    let text = newt_flock::render_for_test(&ev);
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    // a lane cannot start a line of its own
    assert_eq!(text.lines().filter(|l| l.starts_with("lane:")).count(), 1);
    assert!(!text.lines().any(|l| l.starts_with("IGNORE")), "{text}");
}

/// Review 2976 r1 finding 2: work the log already shows as finished is never
/// queued, even though a fresh reader passes through states where the brief
/// is open before its Result frame arrives.
#[test]
fn completed_history_is_not_queued_while_catching_up() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let mut h1 = r.enrol("h1");
    let last = |r: &Rig| {
        r.serving
            .as_ref()
            .unwrap()
            .with_server(|s| s.board().entries().last().unwrap().id)
    };
    to_h1(&mut r, &mut shep, "done");
    let brief = last(&r);
    r.event(&mut h1, Kind::Ack, vec![("of".into(), brief)], Body::Ack);
    r.event(
        &mut h1,
        Kind::Result,
        vec![("brief".into(), brief)],
        Body::Result {
            outcome: Outcome::Completed,
        },
    );
    r.event(
        &mut shep,
        Kind::Review,
        vec![],
        Body::Review {
            to: "h1".into(),
            lane: "done".into(),
            head: "0123abcd".into(),
        },
    );
    let review = last(&r);
    r.event(
        &mut h1,
        Kind::Verdict,
        vec![("review".into(), review)],
        Body::Verdict {
            head: "0123abcd".into(),
            approved: true,
        },
    );
    let s = session_for(&r, "h1", &h1);
    let flock = r.genesis().content_id().unwrap();
    let queue = Queue::default();
    let (mut intake, mut sink) = (Intake::new("h1", flock), queue.clone());
    let inbox = Inbox::start_with(config(&r, &s), move |f| {
        intake.reconcile(f, &mut sink);
    })
    .unwrap();
    until_caught(&inbox);
    drop(inbox);
    assert!(
        queue.tasks.lock().unwrap().is_empty(),
        "finished work was queued from a replay prefix: {:?}",
        queue
            .tasks
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.event_cid.clone())
            .collect::<Vec<_>>()
    );
}

/// Review 2976 r1 finding 5: a callback-only follower keeps no observation
/// queue at all: nothing is buffered for a reader that does not exist.
#[test]
fn a_callback_only_follower_buffers_no_observations() {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let h1 = r.enrol("h1");
    to_h1(&mut r, &mut shep, "a");
    let s = session_for(&r, "h1", &h1);
    let (tx, rx) = mpsc::channel();
    let want = r
        .serving
        .as_ref()
        .unwrap()
        .with_server(|s| s.board().entries().count()) as u64;
    let inbox = Inbox::follow(config(&r, &s), move |f| {
        let _ = tx.send(f.admitted());
    })
    .unwrap();
    // the callback sees the whole log; wait for it (the guard is a watchdog)
    while rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("never caught up")
        < want
    {}
    assert!(
        matches!(
            inbox.events().try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ),
        "an observation queue exists although nothing reads it"
    );
    assert!(matches!(inbox.stop(), newt_flock::Ended::Stopped));
}

/// Review 2976 r2 finding 2: the reader saves every verified frame, so a crash
/// mid-replay leaves a checkpoint that ends before a closing Result/Verdict.
/// Restarting from it must not queue the finished work, and must queue what is
/// genuinely open once this run is caught up.
fn restart_from_a_prefix_ending_at(finished: Kind) {
    let mut r = Rig::new();
    let mut shep = r.enrol("shep");
    let mut h1 = r.enrol("h1");
    let last = |r: &Rig| {
        r.serving
            .as_ref()
            .unwrap()
            .with_server(|s| s.board().entries().last().unwrap().id)
    };
    // finished work, then work that is still open
    if finished == Kind::Brief {
        to_h1(&mut r, &mut shep, "done");
        let brief = last(&r);
        r.event(&mut h1, Kind::Ack, vec![("of".into(), brief)], Body::Ack);
        r.event(
            &mut h1,
            Kind::Result,
            vec![("brief".into(), brief)],
            Body::Result {
                outcome: Outcome::Completed,
            },
        );
    } else {
        let body = |lane: &str| Body::Review {
            to: "h1".into(),
            lane: lane.into(),
            head: "0123abcd".into(),
        };
        r.event(&mut shep, Kind::Review, vec![], body("done"));
        let review = last(&r);
        r.event(
            &mut h1,
            Kind::Verdict,
            vec![("review".into(), review)],
            Body::Verdict {
                head: "0123abcd".into(),
                approved: true,
            },
        );
    }
    to_h1(&mut r, &mut shep, "still-open");
    let s = session_for(&r, "h1", &h1);
    let flock = r.genesis().content_id().unwrap();

    // every verified frame, from a first life with its own state
    let mut first = config(&r, &s);
    first.state_dir = r.tmp.path().join("first-state");
    let inbox = Inbox::start(first).unwrap();
    let mut frames = vec![];
    loop {
        match inbox.events().recv().unwrap() {
            newt_flock::Observed::Frame { frame, .. } => frames.push(frame),
            newt_flock::Observed::Caught(_) => break,
            newt_flock::Observed::Asks(_) => {}
            newt_flock::Observed::Unavailable(w) => panic!("unavailable: {w}"),
        }
    }
    drop(inbox);

    // the crash: a checkpoint holding the log up to the opening event only
    let cfg = config(&r, &s);
    agent_mesh_flock_server::private_dir(&cfg.state_dir).unwrap();
    let mut reader = agent_mesh_flock_session::Reader::open(
        cfg.genesis.clone(),
        &cfg.state_dir,
        agent_mesh_flock_session::RealFs,
    )
    .unwrap();
    for frame in &frames {
        let kinds: Vec<Kind> = frame.iter().map(|s| s.event.payload.kind).collect();
        reader.apply(frame.clone()).unwrap();
        reader.save().unwrap();
        if kinds.contains(&finished) {
            break;
        }
    }
    drop(reader);

    let queue = Queue::default();
    let (mut intake, mut sink) = (Intake::new("h1", flock), queue.clone());
    let (tx, rx) = mpsc::channel();
    let inbox = Inbox::start_with(cfg, move |f| {
        intake.reconcile(f, &mut sink);
        let _ = tx.send(open_tasks(f.board(), "h1").len());
    })
    .unwrap();
    until_caught(&inbox);
    // the worker publishes at catch-up; wait for that publish
    let open = rx.recv().unwrap();
    drop(inbox);
    let queued: Vec<String> = queue
        .tasks
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.body.clone())
        .collect();
    assert_eq!(open, 1, "the log has exactly one open task");
    assert_eq!(
        queued.len(),
        1,
        "the queue and the board must agree after catch-up: {queued:?}"
    );
    assert!(queued[0].contains("still-open"), "{queued:?}");
}

#[test]
fn a_restart_from_a_replay_prefix_ending_at_a_brief_queues_nothing_finished() {
    restart_from_a_prefix_ending_at(Kind::Brief);
}

#[test]
fn a_restart_from_a_replay_prefix_ending_at_a_review_queues_nothing_finished() {
    restart_from_a_prefix_ending_at(Kind::Review);
}
