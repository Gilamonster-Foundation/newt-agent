use super::*;

fn finish(draft: &str, workspace: &str, claims: &TurnClaims<'_>) -> String {
    crate::agentic::finalize_final_text(
        draft.to_string(),
        workspace,
        &Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        claims,
        &crate::agentic::self_verify::VerificationLedger::for_turn("refactor", false),
    )
}

/// Stage 2: the measured refactor removed 94 lines, not the claimed 106.
#[test]
fn report_stage2_corrects_scoped_delta() {
    let dir = fixture();
    std::fs::write(dir.path().join("src/mod.rs"), "x\n".repeat(11726)).unwrap();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([7; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&spill),
            ..Default::default()
        });
    std::fs::write(dir.path().join("src/mod.rs"), "x\n".repeat(11632)).unwrap();
    let reply = finish("src/mod.rs: 11,726 → 11,632 (net −106)", workspace, &claims);
    assert!(reply.contains("[corrected by newt:"), "{reply}");
    assert!(!reply.contains("−106"), "{reply}");
    assert!(reply.contains("-94"), "{reply}");
    assert!(!assistant_prose(&reply).contains("corrected by newt"));
}

/// Stage 2: approximations cannot masquerade as exact observed file sizes.
#[test]
fn report_stage2_marks_approximate_and_ambiguous_sizes() {
    let dir = fixture();
    std::fs::write(dir.path().join("src/mod.rs"), "x\n".repeat(11726)).unwrap();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([7; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&spill),
            ..Default::default()
        });
    for text in [
        "src/mod.rs is the largest file at ~3,800 lines.",
        "mod.rs is 3,800 lines.",
    ] {
        let reply = finish(text, workspace, &claims);
        assert!(reply.contains("[unverified by newt:"), "{reply}");
        assert!(!assistant_prose(&reply).contains("3,800"));
    }
}

/// Stage 2: test totals belong to the named invocation, never a neighbouring log.
#[test]
fn report_stage2_corrects_named_test_total() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([7; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&spill),
            ..Default::default()
        });
    claims.observe_report(workspace, &Scope::All, &Capture {
        command: Some(("cargo test -p newt-core".into(), workspace.into())),
        exit: Some(0),
        lines: vec!["test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s".into()],
        truncated: false,
    }, Some(ExecOutcome::Passed), None);
    let reply = finish(
        "`cargo test -p newt-core` — 1617 tests passed.",
        workspace,
        &claims,
    );
    assert!(reply.contains("[corrected by newt:"), "{reply}");
    assert!(!reply.contains("1617"), "{reply}");
    assert!(reply.contains("1850"), "{reply}");
}

fn review_fixture(text: &str) -> review::Review {
    let dir = fixture();
    let snapshot = files::snapshot(dir.path(), &Scope::All).unwrap();
    review::review(
        text,
        review::Facts {
            before: Some(&snapshot),
            after: Some(&snapshot),
            checks: &[],
            publications: &[],
            root: dir.path(),
        },
    )
}

#[test]
fn report_stage2_leaves_correct_counts_and_non_claim_digits_alone() {
    for text in [
        "src/mod.rs is 2 lines.",
        "1. Explain the change.\n2. Version v1.2.3, issue #123, PR #456, hash deadbeef.",
        "We will reduce src/mod.rs to 1 lines.",
        "`wc -l src/mod.rs`",
        "`echo 1617 tests passed`",
        "> src/mod.rs is 999 lines.\n",
        "```text\nsrc/mod.rs is 999 lines.\n```\n",
        "    src/mod.rs is 999 lines.\n",
    ] {
        let review = review_fixture(text);
        assert!(review.edits.is_empty(), "{text}: {}", review.rendered());
        assert_eq!(review.rendered(), text);
    }
    let reviewed = review_fixture("1. src/mod.rs is 999 lines.").rendered();
    assert!(reviewed.starts_with("1. [corrected by newt:"), "{reviewed}");
}

#[test]
fn report_stage2_audit_roundtrips_and_detects_substitution() {
    let original = "é explains the change.\nsrc/mod.rs is 999 lines.\nUntouched ending.\n";
    let review = review_fixture(original);
    let rendered = review.rendered();
    assert_eq!(review.restore(&rendered).as_deref(), Some(original));
    assert!(review
        .restore(&rendered.replace("2 lines", "3 lines"))
        .is_none());
    let bytes = review.canonical_form().unwrap();
    assert_eq!(
        review.content_id().unwrap(),
        ContentId::from_canonical_bytes(&bytes)
    );
    let mut altered: review::Review =
        serde_json::from_str(&serde_json::to_string(&review).unwrap()).unwrap();
    altered.original.push('x');
    assert_ne!(review.content_id().unwrap(), altered.content_id().unwrap());
}

#[test]
fn report_stage2_missing_facts_and_diffs_are_unverified() {
    for text in [
        "net −106",
        "1617 tests passed.",
        "12 insertions and 8 deletions.",
        "other.rs is 100 lines.",
        "mod.rs is 100 lines.",
    ] {
        let review = review_fixture(text);
        // A complete baseline/current pair can verify an objective-wide net.
        assert!(!review.edits.is_empty(), "{text}");
        if !text.starts_with("net") {
            assert!(review.rendered().contains("[unverified by newt:"), "{text}");
        }
    }
}

#[test]
fn report_stage2_never_borrows_other_command_or_root_and_rejects_partial_totals() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    for (command, cwd, truncated, exit) in [
        ("cargo test -p other", workspace, false, 0),
        ("cargo test -p newt-core", "other/root", false, 0),
        ("cargo test -p newt-core", workspace, true, 0),
        ("cargo test -p newt-core", workspace, false, 124),
    ] {
        let spill = crate::agentic::SessionSpillStore::new([8; 16]);
        let claims =
            TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
                spill: Some(&spill),
                ..Default::default()
            });
        claims.observe_report(workspace, &Scope::All, &Capture {command: Some((command.into(), cwd.into())), exit: Some(exit), truncated,
            lines: vec!["test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s".into()]}, Some(ExecOutcome::Passed), None);
        let output = finish(
            "`cargo test -p newt-core` — 1617 tests passed.",
            workspace,
            &claims,
        );
        assert!(output.contains("[unverified by newt:"), "{output}");
    }
}

#[test]
fn report_stage2_publication_receipts_are_scoped_and_historical() {
    let root = Path::new("project");
    let publications = [
        "PR creation observed: https://github.com/example/project/pull/7".to_string(),
        "Push observed: example/project, branch task, revision abcdef1234567890".to_string(),
    ];
    for (text, expected) in [
        ("PR #7 created.", "unchanged"),
        ("PR #7 created after failed checks.", "unchanged"),
        ("PR #8 created.", "unverified"),
        ("PR #7 is open.", "unverified"),
        ("PR creation blocked.", "corrected"),
        ("I pushed branch `task`.", "unchanged"),
        ("I pushed branch `other`.", "unverified"),
        ("Push failed.", "corrected"),
    ] {
        let r = review::review(
            text,
            review::Facts {
                before: None,
                after: None,
                checks: &[],
                publications: &publications,
                root,
            },
        );
        if expected == "unchanged" {
            assert_eq!(r.rendered(), text);
        } else {
            assert!(
                r.rendered().contains(&format!("[{expected} by newt:")),
                "{text}: {}",
                r.rendered()
            );
        }
    }
}

/// Without retention, keep every original byte represented and qualify it;
/// never silently replace a draft that cannot be recovered.
#[test]
fn report_stage2_without_evidence_retains_original_as_unverified() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(workspace, &Scope::All, None);
    let output = finish("src/mod.rs is 999 lines.", workspace, &claims);
    assert!(
        output.contains("[unverified by newt: src/mod.rs is 999 lines."),
        "{output}"
    );
    assert!(!assistant_prose(&output).contains("999"));
}

#[test]
fn report_stage2_spill_retains_original_and_is_session_fenced() {
    use crate::agentic::{SpillCid, SpillProvenance, SpillStore};
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let store = crate::agentic::SessionSpillStore::new([2; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&store),
            ..Default::default()
        });
    let original = "src/mod.rs is 999 lines.";
    let output = finish(original, workspace, &claims);
    let handle: String = output
        .split("spill:")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let cid = SpillCid::parse(&handle).unwrap();
    let record = store.fetch(&cid).unwrap();
    assert_eq!(SpillCid::of(&record).unwrap(), cid);
    assert_eq!(record.provenance, SpillProvenance::ReportReview);
    let audit: review::Review = serde_json::from_str(&record.redacted_text).unwrap();
    assert_eq!(audit.source_draft, original);
    assert_eq!(audit.restore(&audit.rendered()).as_deref(), Some(original));
    let foreign = crate::agentic::SessionSpillStore::new([3; 16]);
    assert!(foreign.fetch(&cid).is_none());
}

#[test]
fn report_stage2_artifact_chunks_roundtrip_through_verified_existing_store() {
    use crate::agentic::artifact_read::{
        ArtifactReadContext, ArtifactSource, SessionArtifactStore,
    };
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let store = SessionArtifactStore::new("review-test").unwrap();
    let prompt = crate::prompt::PromptId::new();
    let context = ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&store));
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            sink: Some(&store),
            context: Some(context),
            spill: None,
        });
    let original = format!(
        "{}\nsrc/mod.rs is 999 lines.",
        "Long explanation é. ".repeat(4000)
    );
    let output = finish(&original, workspace, &claims);
    assert!(output.contains("[corrected by newt:"));
    let page = store.list_for_prompt(prompt, 0, 100).unwrap();
    assert!(page.records.len() > 1);
    let body: String = page
        .records
        .iter()
        .map(|r| r.body.as_deref().unwrap())
        .collect();
    let audit: review::Review = serde_json::from_str(&body).unwrap();
    assert_eq!(audit.source_draft, original);
    assert_eq!(
        audit.restore(&audit.rendered()).as_deref(),
        Some(original.as_str())
    );
    for (i, record) in page.records.iter().enumerate() {
        assert_eq!(record.metadata["part"], i);
        assert_eq!(
            record.metadata["review_cid"],
            audit.content_id().unwrap().to_string()
        );
        assert_eq!(record.root_prompt_id, prompt);
    }
}

#[test]
fn report_stage2_disclosure_applies_to_audit_and_memory_fetch() {
    use crate::agentic::memory_fetch::{MemAddr, MemPayload, MemorySource, StoreMemorySource};
    use crate::agentic::{SpillCid, SpillStore};
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([12; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&spill),
            ..Default::default()
        });
    let canary = "CANARY-report-review-unique-value";
    let mut disclosure = crate::ocap::DisclosureFilter::new();
    disclosure.register(canary);
    let output = crate::agentic::finalize_final_text(
        format!("{canary}\nsrc/mod.rs is 999 lines."),
        workspace,
        &Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        Some(&disclosure),
        &claims,
        &crate::agentic::self_verify::VerificationLedger::default(),
    );
    assert!(!output.contains(canary));
    let handle: String = output
        .split("spill:")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let record = spill.fetch(&SpillCid::parse(&handle).unwrap()).unwrap();
    assert!(!record.redacted_text.contains(canary));
    let source = StoreMemorySource::from_stores(None, None).with_spill_store(&spill);
    let MemPayload::Found(body) = source.fetch(&MemAddr::Spill { id: handle }).unwrap() else {
        panic!("review must be retrievable")
    };
    let audit: review::Review = serde_json::from_str(&body).unwrap();
    assert!(!audit.source_draft.contains(canary));
    assert!(audit.source_draft.contains("999"));
}

#[test]
fn report_stage2_model_marker_mentions_and_quoted_commands_remain_model_words() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(workspace, &Scope::All, None);
    for original in [
        "[corrected by newt: this is a discussion of the marker]",
        "[unverified by newt: this is an example marker]",
        "Use `echo 1617 tests passed` to illustrate stdout.",
        "The command `echo pushed` prints a word.",
    ] {
        let output = finish(original, workspace, &claims);
        assert_eq!(assistant_prose(&output), original, "{output}");
    }
}

#[test]
fn report_stage2_unsupported_subjects_are_not_false_file_contradictions() {
    for text in [
        "src/mod.rs contains a helper of 20 lines.",
        "In src/mod.rs we reduced a helper by 20 lines.",
        "src/mod.rs is 1,,200 lines.",
        "The difference between old and new src/mod.rs is 20 lines.",
    ] {
        let output = review_fixture(text).rendered();
        assert!(output.contains("[unverified by newt:"), "{output}");
    }
}

#[test]
fn report_stage2_mixed_review_preserves_authored_marker_mentions() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([13; 16]);
    for retain in [false, true] {
        let claims =
            TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
                spill: retain.then_some(&spill),
                ..Default::default()
            });
        for original in [
            "[corrected by newt: an authored discussion]",
            "1. [unverified by newt: an authored discussion]",
            "\\[corrected by newt: an escaped discussion]",
        ] {
            let reply = finish(
                &format!("{original}\nsrc/mod.rs is 999 lines."),
                workspace,
                &claims,
            );
            let model = assistant_prose(&reply);
            assert!(model.contains(original), "{reply} => {model}");
            assert!(!model.contains("999"));
        }
    }
}
