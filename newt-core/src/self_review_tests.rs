//! Shared review consumer controls. These do not claim host capture/dispatch is wired.
use super::*;
use crate::event_journal::Journal;
use content_addressable::{ContentAddressable, ContentId};

fn objective(text: &str) -> ContentId {
    ReviewObjective {
        instruction: text.into(),
        turn_context: "actual-fixture-turn".into(),
    }
    .content_id()
    .unwrap()
}
fn subject(text: &str) -> PresentedSubject {
    PresentedSubject::new(
        objective("review supplied artifact"),
        None,
        ReviewVersions {
            current: [("subject.txt".into(), text.as_bytes().to_vec().into())].into(),
            ..ReviewVersions::default()
        },
        4096,
    )
    .unwrap()
}
fn reply(subject: &PresentedSubject, findings: Vec<&str>) -> String {
    serde_json::json!({
        "subject": subject.content_id().unwrap(),
        "objective": subject.objective(),
        "disposition": if findings.is_empty() { "no_findings" } else { "findings" },
        "findings": findings,
    })
    .to_string()
}

#[test]
fn review_evidence_consumes_fresh_empty_subject_without_claiming_tests_passed() {
    let subject = subject("");
    let mut journal = Journal::new();
    let line = record_reply(&mut journal, &subject, &reply(&subject, vec![])).unwrap();
    let result = consume_evidence(&line, *journal.head().unwrap(), subject.objective(), || {
        Ok(subject.clone())
    })
    .unwrap();
    assert!(
        matches!(result, ReviewStatus::Reviewed { disposition: Assessment::NoFindings, findings } if findings.is_empty())
    );
}

#[test]
fn review_evidence_retains_completed_findings() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    let line = record_reply(
        &mut journal,
        &subject,
        &reply(&subject, vec!["missing bounds check"]),
    )
    .unwrap();
    let result = consume_evidence(&line, *journal.head().unwrap(), subject.objective(), || {
        Ok(subject.clone())
    })
    .unwrap();
    assert!(
        matches!(result, ReviewStatus::Reviewed { disposition: Assessment::Findings, findings } if findings == &["missing bounds check"])
    );
}

#[test]
fn review_evidence_bare_reviewed_is_incomplete_and_history_is_not_rewritten() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    let first = record_reply(&mut journal, &subject, "reviewed").unwrap();
    let first_id = *journal.head().unwrap();
    assert!(matches!(
        first.node.payload().outcome,
        ReviewStatus::Incomplete {
            reason: ReviewFailure::Malformed
        }
    ));
    assert!(consume_evidence(
        &first,
        first_id,
        subject.objective(),
        || Ok(subject.clone())
    )
    .is_err());
    let second = record_reply(&mut journal, &subject, &reply(&subject, vec![])).unwrap();
    assert_eq!(second.parent(), Some(&first_id));
    assert!(first.is_intact());
}

#[test]
fn review_evidence_rejects_tampered_address() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    let mut line = record_reply(&mut journal, &subject, &reply(&subject, vec![])).unwrap();
    let expected = *journal.head().unwrap();
    line.id.push('x');
    assert_eq!(
        consume_evidence(&line, expected, subject.objective(), || Ok(subject.clone())).unwrap_err(),
        ReviewFailure::Tampered
    );
}

#[test]
fn review_evidence_readdressing_cannot_replace_the_retained_phase_head() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    record_reply(&mut journal, &subject, &reply(&subject, vec![])).unwrap();
    let expected = *journal.head().unwrap();
    let mut altered = Journal::new();
    let replacement = record_reply(
        &mut altered,
        &subject,
        &reply(&subject, vec!["different finding"]),
    )
    .unwrap();
    assert!(replacement.is_intact());
    assert_eq!(
        consume_evidence(&replacement, expected, subject.objective(), || Ok(
            subject.clone()
        ))
        .unwrap_err(),
        ReviewFailure::Tampered
    );
}

#[test]
fn review_evidence_rejects_wrong_objective_and_stale_subject() {
    let original = subject("before");
    let mut journal = Journal::new();
    let line = record_reply(&mut journal, &original, &reply(&original, vec![])).unwrap();
    let expected = *journal.head().unwrap();
    assert_eq!(
        consume_evidence(&line, expected, objective("different task"), || Ok(
            original.clone()
        ))
        .unwrap_err(),
        ReviewFailure::WrongObjective
    );
    assert_eq!(
        consume_evidence(&line, expected, original.objective(), || Ok(subject(
            "after"
        )))
        .unwrap_err(),
        ReviewFailure::StaleSubject
    );
}

#[test]
fn review_evidence_cannot_consume_denied_or_incomplete_fresh_capture() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    let line = record_reply(&mut journal, &subject, &reply(&subject, vec![])).unwrap();
    let expected = *journal.head().unwrap();
    for failure in [
        CaptureFailure::Denied,
        CaptureFailure::Incomplete("listing failed".into()),
        CaptureFailure::OverLimit,
        CaptureFailure::Binary,
    ] {
        assert_eq!(
            consume_evidence(
                &line,
                expected,
                subject.objective(),
                || Err(failure.clone())
            )
            .unwrap_err(),
            ReviewFailure::Capture(failure)
        );
    }
}

#[test]
fn review_evidence_rejects_wrong_returned_subject_and_inconsistent_findings() {
    let subject = subject("actual code");
    let mut journal = Journal::new();
    let mut raw: serde_json::Value = serde_json::from_str(&reply(&subject, vec![])).unwrap();
    raw["subject"] = serde_json::to_value(objective("wrong subject")).unwrap();
    let wrong = record_reply(&mut journal, &subject, &raw.to_string()).unwrap();
    assert!(matches!(
        wrong.node.payload().outcome,
        ReviewStatus::Incomplete {
            reason: ReviewFailure::StaleSubject
        }
    ));
    raw["subject"] = serde_json::to_value(subject.content_id().unwrap()).unwrap();
    raw["findings"] = serde_json::json!(["a finding"]);
    let inconsistent = record_reply(&mut journal, &subject, &raw.to_string()).unwrap();
    assert!(matches!(
        inconsistent.node.payload().outcome,
        ReviewStatus::Incomplete {
            reason: ReviewFailure::Malformed
        }
    ));
}

#[test]
fn review_subject_rejects_binary_nul_and_limit_without_partial_success() {
    let id = objective("review");
    assert_eq!(
        PresentedSubject::new(
            id,
            None,
            ReviewVersions {
                current: [("x".into(), b"utf8\0binary".to_vec().into())].into(),
                ..ReviewVersions::default()
            },
            4096
        )
        .unwrap_err(),
        CaptureFailure::Binary
    );
    assert_eq!(
        PresentedSubject::new(
            id,
            None,
            ReviewVersions {
                current: [("x".into(), b"too long".to_vec().into())].into(),
                ..ReviewVersions::default()
            },
            3
        )
        .unwrap_err(),
        CaptureFailure::OverLimit
    );
}

#[test]
fn review_subject_explicit_shape_rejects_unknown_kind_and_empty_artifacts() {
    for invalid in [
        r#"{"kind":"magic"}"#,
        r#"{"kind":"artifacts","paths":[]}"#,
        r#"{"kind":"artifacts","paths":[""]}"#,
        r#"{"kind":"existing_diff","max_rounds":99}"#,
    ] {
        assert!(
            serde_json::from_str::<ReviewSubject>(invalid).is_err(),
            "{invalid}"
        );
    }
    let valid: ReviewSubject =
        serde_json::from_str(r#"{"kind":"artifacts","paths":["source.rs"]}"#).unwrap();
    assert!(matches!(valid, ReviewSubject::Artifacts { paths } if paths == ["source.rs"]));
}

#[test]
fn review_evidence_rejects_self_consistent_but_inconsistent_persisted_assessment() {
    let subject = subject("actual code");
    for (disposition, findings) in [
        (Assessment::NoFindings, vec!["a finding".to_string()]),
        (Assessment::Findings, vec![]),
        (Assessment::Findings, vec!["   ".to_string()]),
    ] {
        let mut journal = Journal::new();
        let line = journal
            .append(ReviewEvent {
                objective: subject.objective(),
                subject: subject.content_id().unwrap(),
                coverage: subject.coverage().clone(),
                outcome: ReviewStatus::Reviewed {
                    disposition,
                    findings,
                },
            })
            .unwrap();
        assert!(line.is_intact());
        let encoded = line.render_line().unwrap();
        let restored = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            consume_evidence(
                &restored,
                *journal.head().unwrap(),
                subject.objective(),
                || Ok(subject.clone())
            )
            .unwrap_err(),
            ReviewFailure::Malformed
        );
    }
}

#[test]
fn review_subject_structural_paths_cannot_alias_display_delimiters() {
    let id = objective("review");
    let first = PresentedSubject::new(
        id,
        None,
        ReviewVersions {
            current: [("a\n=== b ===".into(), b"payload".to_vec().into())].into(),
            ..ReviewVersions::default()
        },
        4096,
    )
    .unwrap();
    let second = PresentedSubject::new(
        id,
        None,
        ReviewVersions {
            current: [("a".into(), b"=== b ===\npayload".to_vec().into())].into(),
            ..ReviewVersions::default()
        },
        4096,
    )
    .unwrap();
    assert_ne!(first.content_id().unwrap(), second.content_id().unwrap());
}

#[test]
fn review_subject_empty_create_delete_and_missing_have_distinct_identity() {
    let id = objective("review");
    let empty = PresentedSubject::new(id, None, ReviewVersions::default(), 4096).unwrap();
    let created = PresentedSubject::new(
        id,
        None,
        ReviewVersions {
            current: [("empty.txt".into(), vec![].into())].into(),
            ..ReviewVersions::default()
        },
        4096,
    )
    .unwrap();
    let deleted = PresentedSubject::new(
        id,
        None,
        ReviewVersions {
            baseline: [("empty.txt".into(), vec![].into())].into(),
            ..ReviewVersions::default()
        },
        4096,
    )
    .unwrap();
    let ids = [
        empty.content_id().unwrap(),
        created.content_id().unwrap(),
        deleted.content_id().unwrap(),
    ];
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
    let mut journal = Journal::new();
    let line = record_reply(&mut journal, &created, &reply(&created, vec![])).unwrap();
    assert_eq!(
        consume_evidence(&line, *journal.head().unwrap(), id, || Ok(deleted.clone())).unwrap_err(),
        ReviewFailure::StaleSubject
    );
}

#[test]
fn review_subject_existing_diff_requires_and_addresses_index_state() {
    let id = objective("review");
    assert!(PresentedSubject::new(
        id,
        Some(ReviewSubject::ExistingDiff),
        ReviewVersions::default(),
        4096
    )
    .is_err());
    let versions = ReviewVersions {
        index: Some(Default::default()),
        current: [("new.txt".into(), b"worktree".to_vec().into())].into(),
        ..ReviewVersions::default()
    };
    let unstaged = PresentedSubject::new(
        id,
        Some(ReviewSubject::ExistingDiff),
        versions.clone(),
        4096,
    )
    .unwrap();
    let mut staged_versions = versions;
    staged_versions.index = Some([("new.txt".into(), b"staged".to_vec().into())].into());
    let staged =
        PresentedSubject::new(id, Some(ReviewSubject::ExistingDiff), staged_versions, 4096)
            .unwrap();
    assert_ne!(unstaged.content_id().unwrap(), staged.content_id().unwrap());
}

#[test]
fn review_subject_serialized_presentation_must_fit_not_only_raw_text() {
    let versions = ReviewVersions {
        current: [("x".into(), vec![b'"'; 200].into())].into(),
        ..ReviewVersions::default()
    };
    let subject = PresentedSubject::new(objective("review"), None, versions.clone(), 4096).unwrap();
    assert!(subject.material().unwrap().len() > 300);
    assert_eq!(
        PresentedSubject::new(objective("review"), None, versions, 300).unwrap_err(),
        CaptureFailure::OverLimit
    );
}
