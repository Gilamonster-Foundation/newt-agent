use super::*;

#[test]
fn offline_replay_rejects_a_missing_composition_decision() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let a = s
        .record_request(
            json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let mut record: RequestRecord = s.store.get(&a.id).unwrap();
    record.composition = Some(ContentId::from_canonical_bytes(b"missing decision"));
    let forged = s.store.put(&record).unwrap();
    assert!(
        crate::replay_from_store(&s.store, forged).is_err(),
        "replay ignored a substituted composition binding"
    );
}

fn fixture() -> (
    tempfile::TempDir,
    Session,
    PreparedRequest,
    ContentId,
    Catalog,
    ContentId,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let a = s
        .record_request(
            json!({"messages":[{"role":"user","content":"old"},{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let (id, c) = s
        .composition_catalog(a.id, Policy { max_bytes: 8192 })
        .unwrap();
    let p = s
        .record_composition_proposal(
            id,
            Actor {
                model: "fixture".into(),
                harness: "test".into(),
            },
            Proposal {
                expected_head: None,
                changes: vec![crate::composition::Change {
                    occurrence: c.entries[0].event,
                    action: Action::Park,
                    reason: "not relevant".into(),
                }],
                inverse: None,
            },
        )
        .unwrap();
    (dir, s, a, id, c, p)
}

/// A crash before publication leaves only the proposal; a crash after atomic
/// replacement restores the complete accepted view. Neither exposes half a view.
#[test]
fn composition_checkpoint_crash_boundaries_keep_old_or_complete_new_view() {
    use crate::store::PublicationFailure::{AfterReplace, BeforeReplace};
    for failure in [BeforeReplace, AfterReplace] {
        let (dir, mut s, a, _, _, proposal) = fixture();
        let old_head = s.head();
        let run = s.run;
        let old_transcript = s.transcript.clone();
        s.store.publication_failure = Some(failure);
        assert!(s.decide_composition(proposal).is_err());
        assert_eq!(
            s.transcript, old_transcript,
            "live state changed before publish returned success"
        );
        assert!(s.composition_bytes().is_err());
        assert!(s.record_request(json!({"messages":[]}), "openai").is_err());
        drop(s);
        let current: ContentId =
            std::fs::read_to_string(dir.path().join("heads").join(run.to_string()))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        assert_eq!(current == old_head, failure == BeforeReplace);
        let mut restored =
            Session::restore(dir.path(), current, &SessionConfig::default().authority).unwrap();
        if failure == BeforeReplace {
            assert_eq!(restored.composition_bytes().unwrap(), a.bytes);
            assert_eq!(restored.composition_head().unwrap(), None);
            assert_eq!(
                restored.decide_composition(proposal).unwrap().outcome,
                Outcome::Accepted
            );
        } else {
            assert_ne!(restored.composition_bytes().unwrap(), a.bytes);
            assert!(restored.composition_head().unwrap().is_some());
        }
        let bytes = restored.composition_bytes().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

/// Staged addressed objects are not capabilities and do not become a published
/// view when the process stops before appending its Composition checkpoint.
#[test]
fn composition_unpublished_staging_and_raw_proposal_resume_without_view_change() {
    let (dir, mut s, a, _, _c, proposal) = fixture();
    let staged = s.stage_composition(proposal).unwrap();
    let decision: Decision = s.store.get(&staged).unwrap();
    let orphan_id = decision.after.unwrap();
    assert!(s.composition_decision(staged).is_err());
    assert!(!s.units.contains_key(&decision.elisions[0].unit));
    let head = s.head();
    drop(s);
    let mut s = Session::restore(dir.path(), head, &SessionConfig::default().authority).unwrap();
    assert!(!s.projections.contains(&orphan_id));
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let r = s.decide_composition(proposal).unwrap();
    assert_eq!(r.projection, orphan_id);
    assert_eq!(r.outcome, Outcome::Accepted);
}

/// Re-addressing forged decision fields cannot turn an illegal candidate into
/// an accepted one: the production admission verifier recomputes the transition.
#[test]
fn composition_verifier_rejects_readdressed_decision_and_receipt_substitution() {
    let (_dir, mut s, _a, _, _, proposal) = fixture();
    let r = s.decide_composition(proposal).unwrap();
    let d = s.composition_decision(r.decision).unwrap();
    let after = d.after.unwrap();
    let mut record: RequestRecord = s.store.get(&s.composition.latest_request.unwrap()).unwrap();
    record.projection = after;
    record.commitment = s.store.put_source(&s.composition_bytes().unwrap()).unwrap();
    record.composition = Some(r.decision);
    assert!(s.verify_composition_request(&record).is_ok());
    record.composition = None;
    assert!(s.verify_composition_request(&record).is_err());
    let mut changed = d.clone();
    changed.outcome = Outcome::Refused(Refusal::Capacity);
    record.composition = Some(s.store.put(&changed).unwrap());
    let forged = s.store.put(&record).unwrap();
    assert!(crate::replay_from_store(&s.store, forged).is_err());
    // Already decided proposal must not mint a second acceptance.
    assert!(s.decide_composition(proposal).is_err());
}

/// Recomputed CIDs alone are insufficient: admission must recompute legal
/// selection, exact bytes and the occurrence-bound elision receipts.
#[test]
fn composition_readdressed_staging_cannot_bypass_admission() {
    let (_dir, mut s, a, _, _, proposal) = fixture();
    let staged = s.stage_composition(proposal).unwrap();
    let original: Decision = s.store.get(&staged).unwrap();
    let mut changed = original.clone();
    changed.elisions.clear();
    let forged = s.store.put(&changed).unwrap();
    assert!(s.publish_composition(forged).is_err());
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let mut changed = original.clone();
    changed.bytes += 1;
    let forged = s.store.put(&changed).unwrap();
    assert!(s.publish_composition(forged).is_err());
    let mut view = s
        .checked_composition_projection(original.after.unwrap())
        .unwrap();
    view.entries.clear();
    let mut changed = original;
    changed.after = Some(s.store.put(&view).unwrap());
    let forged = s.store.put(&changed).unwrap();
    assert!(s.publish_composition(forged).is_err());
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    assert_eq!(s.composition_head().unwrap(), None);
    s.publish_composition(staged).unwrap();
    assert_ne!(s.composition_bytes().unwrap(), a.bytes);
}

/// Transcript checkpoints must restore the same current occurrence vector
/// used by live catalog validation, including host envelopes since the request.
#[test]
fn composition_context_transcript_matches_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
    s.record_request(
        json!({"messages":[{"role":"user","content":"task"}]}),
        "openai",
    )
    .unwrap();
    let messages = vec![
        json!({"role":"system","content":"updated host policy"}),
        json!({"role":"user","content":"task"}),
    ];
    s.record_messages(&messages).unwrap();
    let expected = s.transcript.clone();
    let head = s.head();
    drop(s);
    let restored = Session::restore(dir.path(), head, &SessionConfig::default().authority).unwrap();
    assert_eq!(restored.transcript, expected);
    assert_eq!(restored.restored_messages().unwrap(), messages);
}
