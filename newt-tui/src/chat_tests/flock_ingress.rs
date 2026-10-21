//! Flock-origin ingress at the loop head: which queued rows become a turn,
//! how they are recorded, and that nothing runs on its own.

use super::*;
#[cfg(feature = "flock")]
use crate::flock_intake::provenance_line;
use crate::flock_intake::{next_flock_task, Wake};

fn store() -> (tempfile::TempDir, newt_core::ConversationStore, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store =
        newt_core::ConversationStore::new(tmp.path().join("state"), &workspace, 100).unwrap();
    let conversation = newt_core::new_conversation_id();
    store
        .create_with_id(&conversation, "flock session", None)
        .unwrap();
    (tmp, store, conversation)
}

#[test]
fn the_doorbell_rings_once_per_ring() {
    let wake = Wake::default();
    assert!(!wake.take());
    wake.fire();
    wake.fire();
    assert!(wake.take(), "a ring is seen");
    assert!(!wake.take(), "and consumed");
}

#[test]
fn a_flock_origin_is_not_operator_input() {
    let (_t, store, conv) = store();
    let task = newt_core::NewFlockTask {
        body: "b".into(),
        flock_cid: "f".into(),
        event_cid: "e".into(),
        evidence_cid: "c".into(),
        evidence: "{}".into(),
    };
    store.enqueue_flock_task(&conv, &task).unwrap();
    let row = store.peek_flock_task(&conv).unwrap().unwrap();
    let origin = ModelInputOrigin::FlockTask {
        task: Box::new(row),
    };
    assert!(
        !origin.is_operator(),
        "a signed brief is inert model text: no slash command, no `!` escape, no history"
    );
}

#[cfg(not(feature = "flock"))]
#[test]
fn a_build_that_cannot_verify_refuses_every_row_and_never_offers_it() {
    let (_t, store, conv) = store();
    for e in ["e1", "e2"] {
        store
            .enqueue_flock_task(
                &conv,
                &newt_core::NewFlockTask {
                    body: "b".into(),
                    flock_cid: "f".into(),
                    event_cid: e.into(),
                    evidence_cid: "c".into(),
                    evidence: "{}".into(),
                },
            )
            .unwrap();
    }
    let next = next_flock_task(&store, &conv);
    assert!(next.task.is_none());
    assert_eq!(next.rejected.len(), 2);
    assert_eq!(store.peek_flock_task(&conv).unwrap(), None);
}

#[cfg(feature = "flock")]
mod with_flock {
    use super::*;
    use newt_flock::{CitedFile, Evidence, Task, TaskKind};

    fn evidence(event: &str, kind: TaskKind, head: Option<&str>) -> Evidence {
        Evidence {
            flock: "flock-1".into(),
            event: event.into(),
            index: 9,
            kind,
            from: "shep".into(),
            to: "h1".into(),
            lane: "lane-a".into(),
            head: head.map(Into::into),
            files: vec![CitedFile {
                root: "notes".into(),
                path: "brief.md".into(),
                id: "raw".into(),
            }],
        }
    }

    fn queued(store: &newt_core::ConversationStore, conv: &str, ev: Evidence) {
        let n = Task { evidence: ev }.to_new().unwrap();
        store
            .enqueue_flock_task(
                conv,
                &newt_core::NewFlockTask {
                    body: n.body,
                    flock_cid: n.flock_cid,
                    event_cid: n.event_cid,
                    evidence_cid: n.evidence_cid,
                    evidence: n.evidence,
                },
            )
            .unwrap();
    }

    fn accept(store: &newt_core::ConversationStore, conv: &str, row: &newt_core::FlockTask) {
        let eph = newt_core::agentic::SessionPromptStore::default();
        let ctx = begin_model_prompt(
            PromptIngress {
                durable: Some(store),
                ephemeral: &eph,
            },
            conv,
            "flock session",
            None,
            row.task.body.as_bytes(),
            row.task.body.as_bytes(),
            &ModelInputOrigin::FlockTask {
                task: Box::new(row.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            ctx.submitted_prompt().receipt().origin(),
            newt_core::PromptOrigin::Flock,
            "a flock task is never recorded as operator input"
        );
        ctx.submitted_prompt().receipt().verify_integrity().unwrap();
    }

    #[test]
    fn tasks_are_offered_in_order_one_at_a_time_and_stay_queued_until_accepted() {
        let (_t, store, conv) = store();
        queued(&store, &conv, evidence("e1", TaskKind::Brief, None));
        queued(&store, &conv, evidence("e2", TaskKind::Review, Some("abc")));
        let first = next_flock_task(&store, &conv).task.unwrap();
        assert_eq!(first.task.event_cid, "e1");
        // busy: nothing has accepted it, so asking again changes nothing
        assert_eq!(
            next_flock_task(&store, &conv).task.unwrap().id,
            first.id,
            "an unaccepted task is offered again, not skipped"
        );
        accept(&store, &conv, &first);
        let second = next_flock_task(&store, &conv).task.unwrap();
        assert_eq!(second.task.event_cid, "e2");
        accept(&store, &conv, &second);
        assert!(next_flock_task(&store, &conv).task.is_none());
        assert!(provenance_line(&second).contains("flock"));
    }

    #[test]
    fn a_review_keeps_its_exact_head_from_evidence_to_the_prompt_text() {
        let (_t, store, conv) = store();
        queued(
            &store,
            &conv,
            evidence("e1", TaskKind::Review, Some("0123abcd")),
        );
        let row = next_flock_task(&store, &conv).task.unwrap();
        assert!(row.task.body.contains("head under review: 0123abcd"));
        let ev: Evidence = serde_json::from_str(&row.task.evidence).unwrap();
        assert_eq!(ev.head.as_deref(), Some("0123abcd"));
    }

    #[test]
    fn a_row_edited_in_place_is_refused_and_never_offered_again_and_the_next_is() {
        let (_t, store, conv) = store();
        // what a hand edit of the stored head leaves behind: evidence that
        // no longer hashes to the content id it was queued with
        let mut n = Task {
            evidence: evidence("bad", TaskKind::Review, Some("0123abcd")),
        }
        .to_new()
        .unwrap();
        n.evidence = n.evidence.replace("0123abcd", "ffffffff");
        store
            .enqueue_flock_task(
                &conv,
                &newt_core::NewFlockTask {
                    body: n.body,
                    flock_cid: n.flock_cid,
                    event_cid: n.event_cid,
                    evidence_cid: n.evidence_cid,
                    evidence: n.evidence,
                },
            )
            .unwrap();
        queued(&store, &conv, evidence("good", TaskKind::Brief, None));
        let next = next_flock_task(&store, &conv);
        assert_eq!(next.rejected.len(), 1, "{:?}", next.rejected);
        assert_eq!(next.task.unwrap().task.event_cid, "good");
        // the refused one never comes back, and its event stays deduped
        assert_eq!(next_flock_task(&store, &conv).rejected.len(), 0);
    }
}
