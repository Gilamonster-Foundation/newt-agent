use super::*;

/// #1671: the pure name-matching rules behind `--resume <name>` — a unique
/// exact (case-insensitive) title wins, then a unique substring; ambiguity
/// and misses are hard errors that NAME the candidates.
#[test]
fn resume_by_name_matches_titles_and_refuses_ambiguity() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];

    // Case-insensitive EXACT match wins even when a superstring also exists.
    assert_eq!(
        resolve_conversation_by_name(&list, "Mesh Docking").unwrap(),
        "aaaa1111-0000-0000-0000-000000000000"
    );
    // Unique substring match resolves.
    assert_eq!(
        resolve_conversation_by_name(&list, "taxes").unwrap(),
        "cccc3333-0000-0000-0000-000000000000"
    );
    // Ambiguous substring: hard error listing the candidates.
    let err = resolve_conversation_by_name(&list, "docking")
        .unwrap_err()
        .to_string();
    assert!(err.contains("matches 2 conversations"), "{err}");
    assert!(err.contains("mesh docking"), "{err}");
    // No match: hard error, pointing at /resume browse.
    let err = resolve_conversation_by_name(&list, "nonesuch")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no conversation titled"), "{err}");
}

/// #1736: the consolidated `resolve_resume_target` is the ONE precedence chain
/// shared by startup `--resume <name>` and in-chat `/resume <thing>`. This is
/// the pure core — no store — so every resolution rule is unit-testable, and
/// because both front doors call it, equivalence between them is structural.
#[test]
fn resolve_resume_target_chains_id_prefix_title_then_ambiguity() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];

    // 1. exact conversation id.
    assert_eq!(
        crate::resolve_resume_target(&list, "cccc3333-0000-0000-0000-000000000000"),
        crate::ResumeNameResolve::Resolved("cccc3333-0000-0000-0000-000000000000".into())
    );
    // 2. unique id prefix.
    assert_eq!(
        crate::resolve_resume_target(&list, "aaaa1111"),
        crate::ResumeNameResolve::Resolved("aaaa1111-0000-0000-0000-000000000000".into())
    );
    // 3. exact (case-insensitive) title wins over the superstring sibling.
    assert_eq!(
        crate::resolve_resume_target(&list, "mesh docking"),
        crate::ResumeNameResolve::Resolved("aaaa1111-0000-0000-0000-000000000000".into())
    );
    // 4. unique title substring.
    assert_eq!(
        crate::resolve_resume_target(&list, "taxes"),
        crate::ResumeNameResolve::Resolved("cccc3333-0000-0000-0000-000000000000".into())
    );
    // 5. ambiguous title match → candidates for numbered selection.
    let amb = crate::resolve_resume_target(&list, "docking");
    let cands = match amb {
        crate::ResumeNameResolve::Ambiguous(c) => c,
        other => panic!("expected Ambiguous, got {other:?}"),
    };
    assert_eq!(cands.len(), 2);
    assert!(cands
        .iter()
        .any(|(id, _)| id == "aaaa1111-0000-0000-0000-000000000000"));
    assert!(cands
        .iter()
        .any(|(id, _)| id == "bbbb2222-0000-0000-0000-000000000000"));
    // 6. nothing matched → NotFound (the in-chat caller falls back to FTS).
    assert_eq!(
        crate::resolve_resume_target(&list, "nonesuch"),
        crate::ResumeNameResolve::NotFound
    );
    // A non-unique id prefix is NOT a silent resume — it falls through to
    // title matching, and with no title match it is NotFound (never a guess).
    let shared_prefix = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "one"),
        s("aaaa2222-0000-0000-0000-000000000000", "two"),
    ];
    assert_eq!(
        crate::resolve_resume_target(&shared_prefix, "aaaa"),
        crate::ResumeNameResolve::NotFound
    );
}

/// #1736: the title step of `resolve_resume_target` DELEGATES to
/// `resolve_conversation_by_name`, so the two front doors can never drift.
/// Every title query that resolves through the consolidated resolver must
/// resolve identically through the title-only startup resolver.
#[test]
fn resolve_resume_target_agrees_with_resolve_conversation_by_name_on_titles() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];
    for q in [
        "mesh docking",
        "Mesh Docking",
        "taxes",
        "docking",
        "nonesuch",
    ] {
        let via_target = crate::resolve_resume_target(&list, q);
        let via_name = resolve_conversation_by_name(&list, q);
        match (&via_target, via_name) {
            (crate::ResumeNameResolve::Resolved(a), Ok(b)) => assert_eq!(a, &b, "title {q:?}"),
            (
                crate::ResumeNameResolve::Ambiguous(a),
                Err(crate::TitleResolveError::Ambiguous { candidates: b, .. }),
            ) => {
                let a_ids: Vec<_> = a.iter().map(|(id, _)| id.as_str()).collect();
                let b_ids: Vec<_> = b.iter().map(|(id, _)| id.as_str()).collect();
                assert_eq!(a_ids, b_ids, "ambiguous title {q:?}");
            }
            (
                crate::ResumeNameResolve::NotFound,
                Err(crate::TitleResolveError::NotFound { .. }),
            ) => {}
            other => panic!("disagreement on {q:?}: {other:?}"),
        }
    }
}

/// #1736: an ambiguous `/resume <thing>` renders a NUMBERED, liveness-annotated
/// candidate listing so a follow-up `/resume <n>` selects one — not a bare
/// error. Mirrors the browse/search listing tests.
#[serial_test::serial(real_fs)]
#[test]
fn resume_ambiguous_message_numbers_candidates_for_selection() {
    let (_state, _ws, store) = recall_test_store();
    let a = store.create("mesh docking", None).unwrap();
    let b = store.create("Mesh Docking Ceremony", None).unwrap();
    let cands = vec![
        (a.clone(), "mesh docking".to_string()),
        (b.clone(), "Mesh Docking Ceremony".to_string()),
    ];
    let (msg, ids) = resume_ambiguous_message(&store, "docking", &cands, "other-active").unwrap();
    assert_eq!(ids, vec![a.clone(), b.clone()]);
    assert!(msg.contains("\"docking\" matches 2 conversations"), "{msg}");
    assert!(msg.contains("1. "), "numbered: {msg}");
    assert!(msg.contains("2. "), "numbered: {msg}");
    assert!(msg.contains("mesh docking") && msg.contains("Mesh Docking Ceremony"));
}

/// #1736: the live-owner/concurrent-newt protection is the claim-guard
/// `/resume` consults before reopening. A conversation a live newt already
/// holds must report `HeldBy` (so `/resume` refuses) — never `Claimed`.
#[serial_test::serial(real_fs)]
#[test]
fn resume_refuses_a_conversation_a_live_newt_owns() {
    let (_state, _ws, mut store) = recall_test_store();
    let id = store.create("Held by another", None).unwrap();
    // Plant a FOREIGN live owner (host A), then switch this store's identity
    // to a second newt (host B) and re-claim — the guard the `/resume` path
    // consults. A live, different owner must be refused with `HeldBy`.
    store.set_owner_for_test("hostA", "bootA", 1);
    store.set_liveness_for_test(|_, _| true);
    assert_eq!(
        store.claim(&id).unwrap(),
        newt_core::ClaimOutcome::Claimed,
        "first claim by host A should acquire"
    );
    store.set_owner_for_test("hostB", "bootB", 2);
    match store.claim(&id) {
        Ok(newt_core::ClaimOutcome::HeldBy { host, pid }) => {
            assert_eq!(host, "hostA");
            assert_eq!(pid, 1);
        }
        other => panic!("expected HeldBy for a live-owned conversation, got {other:?}"),
    }
}

#[serial_test::serial(real_fs)]
#[test]
fn should_auto_resume_only_for_latest_and_never_after_new() {
    // Config off / ephemeral / exact-id sessions never auto-resume.
    assert!(should_auto_resume(&SessionStart::ResumeLatest, false));
    assert!(!should_auto_resume(&SessionStart::Fresh, false));
    assert!(!should_auto_resume(&SessionStart::Ephemeral, false));
    assert!(!should_auto_resume(
        &SessionStart::ResumeExact("id".into()),
        false
    ));
    // /new opts the session out — auto-resume never undoes it.
    assert!(!should_auto_resume(&SessionStart::ResumeLatest, true));
}
