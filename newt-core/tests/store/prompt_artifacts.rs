use super::*;

// =========================================================================
// Prompt-rooted artifact ledger: bounded derived-work lineage, not a second
// transcript. Every read verifies the immutable per-conversation chain.
// =========================================================================

#[test]
fn prompt_artifacts_preserve_order_direct_prompt_and_objective_root() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "artifact-lineage";
    let root_prompt = store
        .begin_prompt(
            conversation_id,
            "artifact lineage",
            None,
            NewPrompt::operator("build it", "build it"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let plan = store
        .append_prompt_artifact(
            conversation_id,
            root_prompt.id(),
            NewPromptArtifact::new(ArtifactKind::PlanRevision, ArtifactRelation::DerivedFrom)
                .with_body("1. inspect\n2. implement")
                .with_metadata(serde_json::json!({"revision": 1})),
        )
        .unwrap();

    let continuation = store
        .begin_prompt(
            conversation_id,
            "artifact lineage",
            None,
            NewPrompt::operator_continuation(
                "also update docs",
                "also update docs",
                root_prompt.id(),
            ),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let file = store
        .append_prompt_artifact(
            conversation_id,
            continuation.id(),
            NewPromptArtifact::new(ArtifactKind::FileChange, ArtifactRelation::Realizes)
                .with_locator("docs/design.md")
                .with_metadata(serde_json::json!({
                    "before_digest": "abc",
                    "after_digest": "def"
                })),
        )
        .unwrap();

    assert_eq!(plan.seq(), 1);
    assert_eq!(file.seq(), 2);
    assert_eq!(file.prev_hash(), plan.artifact_hash());
    assert_eq!(plan.prompt_id(), root_prompt.id());
    assert_eq!(file.prompt_id(), continuation.id());
    assert_eq!(plan.root_prompt_id(), root_prompt.id());
    assert_eq!(file.root_prompt_id(), root_prompt.id());
    assert_eq!(plan.body(), Some("1. inspect\n2. implement"));
    assert_eq!(file.locator(), Some("docs/design.md"));
    plan.verify_integrity().unwrap();
    file.verify_integrity().unwrap();
    store.verify_prompt_artifact_chain(conversation_id).unwrap();

    let (first_page, total) = store
        .page_prompt_artifacts_for_root(conversation_id, root_prompt.id(), 0, 1)
        .unwrap();
    assert_eq!(total, 2, "the page total is not inferred from page length");
    assert_eq!(first_page, vec![plan.clone()]);
    assert_eq!(
        store
            .list_prompt_artifacts_for_prompt(conversation_id, continuation.id(), 0, 10)
            .unwrap(),
        vec![file.clone()]
    );
    assert_eq!(
        store
            .load_prompt_artifact(conversation_id, file.id())
            .unwrap(),
        Some(file)
    );
}

#[test]
fn prompt_artifact_bounds_fail_before_any_row_is_committed() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "artifact-bounds";
    let prompt_id = store
        .begin_prompt(
            conversation_id,
            "bounds",
            None,
            NewPrompt::operator("bounded", "bounded"),
        )
        .unwrap()
        .submitted()
        .id();

    for invalid in [
        NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom)
            .with_body("x".repeat(MAX_ARTIFACT_BODY_BYTES + 1)),
        NewPromptArtifact::new(ArtifactKind::FileChange, ArtifactRelation::Realizes)
            .with_locator("x".repeat(MAX_ARTIFACT_LOCATOR_BYTES + 1)),
        NewPromptArtifact::new(ArtifactKind::TurnOutcome, ArtifactRelation::Summarizes)
            .with_metadata(serde_json::json!({
                "oversized": "x".repeat(MAX_ARTIFACT_METADATA_BYTES)
            })),
        NewPromptArtifact::new(ArtifactKind::TurnOutcome, ArtifactRelation::Summarizes)
            .with_metadata(serde_json::json!(["raw", "stream"])),
    ] {
        assert!(store
            .append_prompt_artifact(conversation_id, prompt_id, invalid)
            .is_err());
    }
    assert_eq!(store.count_prompt_artifacts(conversation_id).unwrap(), 0);
    let rows: i64 = raw(root.path())
        .query_row("SELECT COUNT(*) FROM prompt_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn prompt_artifact_reads_and_writes_are_conversation_and_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let workspace_a = tempfile::tempdir().unwrap();
    let workspace_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), workspace_a.path(), 100).unwrap();
    let prompt_a = store_a
        .begin_prompt("artifact-a", "a", None, NewPrompt::operator("a", "a"))
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let artifact_a = store_a
        .append_prompt_artifact(
            "artifact-a",
            prompt_a.id(),
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
        )
        .unwrap();

    let store_b = ConversationStore::new(root.path(), workspace_b.path(), 100).unwrap();
    let prompt_b = store_b
        .begin_prompt("artifact-b", "b", None, NewPrompt::operator("b", "b"))
        .unwrap()
        .submitted()
        .receipt()
        .clone();

    assert!(store_b
        .append_prompt_artifact(
            "artifact-a",
            prompt_a.id(),
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
        )
        .is_err());
    assert!(store_a
        .append_prompt_artifact(
            "artifact-a",
            prompt_b.id(),
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
        )
        .is_err());
    assert_eq!(
        store_b
            .load_prompt_artifact("artifact-a", artifact_a.id())
            .unwrap(),
        None
    );
    assert!(store_b
        .list_prompt_artifacts_for_prompt("artifact-a", prompt_a.id(), 0, 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        store_a
            .load_prompt_artifact("artifact-b", artifact_a.id())
            .unwrap(),
        None
    );
}

#[test]
fn prompt_artifact_chain_detects_content_rewrite_and_tail_truncation() {
    let make_store = || {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let prompt = store
            .begin_prompt(
                "artifact-tamper",
                "tamper",
                None,
                NewPrompt::operator("tamper", "tamper"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        for body in ["first", "second"] {
            store
                .append_prompt_artifact(
                    "artifact-tamper",
                    prompt.id(),
                    NewPromptArtifact::new(ArtifactKind::PlanRevision, ArtifactRelation::Updates)
                        .with_body(body),
                )
                .unwrap();
        }
        (root, workspace, store, prompt.id())
    };

    let (root, _workspace, store, prompt_id) = make_store();
    raw(root.path())
        .execute(
            "UPDATE prompt_artifacts SET body = 'rewritten' WHERE seq = 1",
            [],
        )
        .unwrap();
    let error = store
        .verify_prompt_artifact_chain("artifact-tamper")
        .unwrap_err()
        .to_string();
    assert!(error.contains("hash mismatch"), "{error}");
    assert!(store
        .append_prompt_artifact(
            "artifact-tamper",
            prompt_id,
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
        )
        .is_err());

    let (root, _workspace, store, _) = make_store();
    raw(root.path())
        .execute("DELETE FROM prompt_artifacts WHERE seq = 2", [])
        .unwrap();
    let error = store
        .verify_prompt_artifact_chain("artifact-tamper")
        .unwrap_err()
        .to_string();
    assert!(error.contains("chain tip mismatch"), "{error}");
}

#[test]
fn prompt_artifacts_fail_closed_when_prompt_chronology_is_tampered() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "artifact-prompt-order-tamper";
    store
        .begin_prompt(
            conversation_id,
            "first",
            None,
            NewPrompt::operator("first", "first"),
        )
        .unwrap();
    let second = store
        .begin_prompt(
            conversation_id,
            "second",
            None,
            NewPrompt::operator("second", "second"),
        )
        .unwrap()
        .submitted()
        .id();
    let artifact = store
        .append_prompt_artifact(
            conversation_id,
            second,
            NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
        )
        .unwrap();

    raw(root.path())
        .execute(
            "UPDATE prompt_receipts SET receipt_order = 0 WHERE id = ?1",
            [second.to_string()],
        )
        .unwrap();

    for error in [
        store
            .verify_prompt_artifact_chain(conversation_id)
            .unwrap_err()
            .to_string(),
        store
            .load_prompt_artifact(conversation_id, artifact.id())
            .unwrap_err()
            .to_string(),
        store
            .append_prompt_artifact(
                conversation_id,
                second,
                NewPromptArtifact::new(ArtifactKind::Decision, ArtifactRelation::DerivedFrom),
            )
            .unwrap_err()
            .to_string(),
    ] {
        assert!(error.contains("prompt chronology mismatch"), "{error}");
    }
}

#[test]
fn deleting_a_conversation_cascades_prompt_artifacts_and_chain_tip() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "artifact-cascade";
    let prompt_id = store
        .begin_prompt(
            conversation_id,
            "cascade",
            None,
            NewPrompt::operator("delete later", "delete later"),
        )
        .unwrap()
        .submitted()
        .id();
    store
        .append_prompt_artifact(
            conversation_id,
            prompt_id,
            NewPromptArtifact::new(ArtifactKind::TurnOutcome, ArtifactRelation::Summarizes),
        )
        .unwrap();

    store.delete(conversation_id).unwrap();
    let conn = raw(root.path());
    let artifacts: i64 = conn
        .query_row("SELECT COUNT(*) FROM prompt_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    let tips: i64 = conn
        .query_row("SELECT COUNT(*) FROM prompt_artifact_tips", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!((artifacts, tips), (0, 0));
}

#[test]
fn concurrent_artifact_appends_serialize_one_conversation_chain() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let seed = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "artifact-concurrency";
    let prompt_id = seed
        .begin_prompt(
            conversation_id,
            "concurrency",
            None,
            NewPrompt::operator("parallel", "parallel"),
        )
        .unwrap()
        .submitted()
        .id();
    drop(seed);

    let workers: Vec<_> = (0..2)
        .map(|worker| {
            let root = root.path().to_path_buf();
            let workspace = workspace.path().to_path_buf();
            std::thread::spawn(move || {
                let store = ConversationStore::new(&root, &workspace, 100).unwrap();
                store
                    .append_prompt_artifact(
                        conversation_id,
                        prompt_id,
                        NewPromptArtifact::new(
                            ArtifactKind::Decision,
                            ArtifactRelation::DerivedFrom,
                        )
                        .with_metadata(serde_json::json!({"worker": worker})),
                    )
                    .unwrap()
            })
        })
        .collect();
    let mut appended: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    appended.sort_by_key(|artifact| artifact.seq());
    assert_eq!(
        appended
            .iter()
            .map(|artifact| artifact.seq())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(appended[1].prev_hash(), appended[0].artifact_hash());

    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    reopened
        .verify_prompt_artifact_chain(conversation_id)
        .unwrap();
    assert_eq!(reopened.count_prompt_artifacts(conversation_id).unwrap(), 2);
}
