use agent_harness::{
    forensics::{inspect_from_store, InspectionLimits},
    store::FrameStore,
    Session, Verdict,
};

/// Grounds read-only journal discovery in real files after the writer exits.
#[test]
fn journal_heads_expose_verdict_and_request_links_without_granting_authority() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), Default::default()).unwrap();
    let request = session
        .record_request(
            serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let reply = session
        .record_reply(request.id, b"observed answer")
        .unwrap();
    session.record_verdict(reply, Verdict::Answer).unwrap();
    let head = session.head();
    let run = session.run_id();
    let retrieval = session.re_read(&reply.to_string(), 0, 4).unwrap();
    let checkpoint = session.checkpoint_path().unwrap();
    let before = std::fs::read(&checkpoint).unwrap();
    drop(session);
    let store = FrameStore::open(dir.path()).unwrap();
    let journal = inspect_from_store(&store, head, InspectionLimits::default())
        .unwrap()
        .unwrap();
    assert_eq!(journal.kind, "journal");
    assert!(!journal.graph_admitted);
    assert_eq!(journal.record["verdict"]["reply"], reply.to_string());
    assert_eq!(journal.parents.len(), 1);
    for (id, kind) in [
        (request.id, "request"),
        (request.projection, "projection"),
        (
            retrieval["retrieval"].as_str().unwrap().parse().unwrap(),
            "retrieval",
        ),
    ] {
        assert_eq!(
            inspect_from_store(&store, id, InspectionLimits::default())
                .unwrap()
                .unwrap()
                .kind,
            kind
        );
    }
    let run = inspect_from_store(&store, run, InspectionLimits::default())
        .unwrap()
        .unwrap();
    let root = run.record["run"]["root"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        inspect_from_store(&store, root, InspectionLimits::default())
            .unwrap()
            .unwrap()
            .kind,
        "root"
    );
    let event = inspect_from_store(&store, reply, InspectionLimits::default())
        .unwrap()
        .unwrap();
    assert_eq!(event.kind, "event");
    assert_eq!(event.record["origin"], "model");
    assert_eq!(std::fs::read(&checkpoint).unwrap(), before);
    assert!(inspect_from_store(
        &store,
        head,
        InspectionLimits {
            max_bytes: 1,
            ..Default::default()
        }
    )
    .is_err());
    assert!(inspect_from_store(
        &store,
        head,
        InspectionLimits {
            max_references: 1,
            ..Default::default()
        }
    )
    .is_err());
    std::fs::remove_file(dir.path().join(format!("{reply}.cbor"))).unwrap();
    assert!(inspect_from_store(&store, head, InspectionLimits::default()).is_err());
}
