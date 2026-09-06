use super::*;

fn prompt_store() -> (tempfile::TempDir, newt_core::ConversationStore, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store =
        newt_core::ConversationStore::new(tmp.path().join("state"), &workspace, 100).unwrap();
    (tmp, store, newt_core::new_conversation_id())
}

#[test]
fn session_artifact_store_exists_only_for_ephemeral_mode() {
    let conversation_id = newt_core::new_conversation_id();

    assert!(session_artifact_store(false, &conversation_id)
        .unwrap()
        .is_none());
    let ephemeral = session_artifact_store(true, &conversation_id)
        .unwrap()
        .expect("ephemeral ledger");
    assert_eq!(ephemeral.conversation_id(), conversation_id);
}

#[test]
fn session_artifact_store_rebinds_when_conversation_rotates() {
    let first_id = newt_core::new_conversation_id();
    let second_id = newt_core::new_conversation_id();
    let mut ledger = session_artifact_store(true, &first_id)
        .unwrap()
        .expect("first ledger");
    assert_eq!(ledger.conversation_id(), first_id);

    ledger = session_artifact_store(true, &second_id)
        .unwrap()
        .expect("replacement ledger");
    assert_eq!(ledger.conversation_id(), second_id);
    assert_ne!(ledger.conversation_id(), first_id);
}

#[test]
fn git_head_snapshot_requires_effective_workspace_read_authority() {
    let tmp = tempfile::TempDir::new().unwrap();
    let tool = newt_git::LocalGitTool {
        root: tmp.path().to_path_buf(),
        author: newt_git::Author {
            name: "test".into(),
            email: "test@example.com".into(),
        },
        attribution: None,
        commit_succeeded: std::sync::atomic::AtomicUsize::new(0),
        contributors_consumed: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut denied = newt_core::Caveats::top();
    denied.fs_read = newt_core::Scope::none();
    assert!(git_head_snapshot(Some(&tool), &denied).is_none());
    assert!(git_head_snapshot(None, &newt_core::Caveats::top()).is_none());
}

#[test]
fn persistent_ingress_keeps_exact_raw_and_model_bytes_in_prompt_only_conversation() {
    let (_tmp, store, conversation_id) = prompt_store();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let raw = b"  inspect src \\\nthen test  ";
    let model = b"inspect src \nthen test";

    let context = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "inspect src",
        Some("coder"),
        raw,
        model,
        &ModelInputOrigin::Operator,
    )
    .unwrap();

    let submitted = context.submitted_prompt().receipt();
    assert_eq!(submitted.raw_text(), raw);
    assert_eq!(submitted.model_text(), model);
    assert_eq!(submitted.origin(), newt_core::PromptOrigin::Operator);
    assert_eq!(submitted.root_prompt_id(), submitted.id());
    submitted.verify_integrity().unwrap();

    let record = store.load(&conversation_id).unwrap();
    assert!(record.turns.is_empty(), "ingress precedes turn completion");
    assert_eq!(record.title, "inspect src");
    assert_eq!(record.persona.as_deref(), Some("coder"));
}

/// A3/W6 end-to-end at the seam level: a prompt enqueued via the attach
/// API is dequeued and minted by the running session (D2), its durable
/// receipt stays `origin=operator` (so no prompt_receipts CHECK migration),
/// the inbox row is linked to that receipt, and — crucially — a web-injected
/// line is NOT operator input to the TUI (its `/exit`/`!rm` can never reach
/// the host-shell/slash/history gates).
#[test]
fn web_injected_prompt_mints_operator_receipt_and_is_inert_tui_input() {
    // Containment: the load-bearing safety property.
    let origin = ModelInputOrigin::WebInjected {
        inbox_id: "ib-1".into(),
    };
    assert!(
        !origin.is_operator(),
        "a web-injected line must be inert model text, never TUI operator input"
    );

    let (_tmp, store, conversation_id) = prompt_store();
    store
        .create_with_id(&conversation_id, "attached session", None)
        .unwrap();
    // The attach surface enqueues; it never mints the turn.
    store
        .inject_prompt(&conversation_id, "fix the flaky test", Some("req-1"))
        .unwrap();
    let injected = store
        .take_injected_prompt(&conversation_id)
        .unwrap()
        .expect("one queued prompt");
    assert_eq!(injected.body, "fix the flaky test");

    // The RUNNING session mints the turn (D2), durable origin = operator.
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let context = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "attached session",
        None,
        injected.body.as_bytes(),
        injected.body.as_bytes(),
        &ModelInputOrigin::WebInjected {
            inbox_id: injected.id.clone(),
        },
    )
    .unwrap();
    let submitted = context.submitted_prompt().receipt();
    assert_eq!(
        submitted.origin(),
        newt_core::PromptOrigin::Operator,
        "durable origin stays operator — no prompt_receipts CHECK migration"
    );
    submitted.verify_integrity().unwrap();

    // Additive provenance + exactly-once drain.
    store
        .link_inbox_delivery(&injected.id, &submitted.id().to_string())
        .unwrap();
    assert_eq!(
        store.take_injected_prompt(&conversation_id).unwrap(),
        None,
        "the inbox drained exactly once"
    );
}

#[test]
fn persistent_ingress_error_is_returned_before_a_prompt_can_run() {
    let (_tmp, store, _conversation_id) = prompt_store();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let err = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        "../outside",
        "unsafe id",
        None,
        b"do work",
        b"do work",
        &ModelInputOrigin::Operator,
    )
    .unwrap_err();

    assert!(err.to_string().contains("invalid"), "got: {err}");
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn ephemeral_ingress_has_equivalent_context_without_a_store() {
    let conversation_id = newt_core::new_conversation_id();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let context = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "unused",
        None,
        b" raw ",
        b"raw",
        &ModelInputOrigin::Operator,
    )
    .unwrap();

    assert_eq!(context.submitted_prompt().receipt().raw_text(), b" raw ");
    assert_eq!(context.active_operator_prompt().model_text(), b"raw");
    assert_eq!(
        context.submitted_prompt().receipt().origin(),
        newt_core::PromptOrigin::Operator
    );
}

#[test]
fn ephemeral_ingress_chains_attempts_and_fences_a_new_conversation() {
    use newt_core::agentic::PromptSource as _;

    let prompts = newt_core::agentic::SessionPromptStore::default();
    let first_conversation = newt_core::new_conversation_id();
    let first = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &prompts,
        },
        &first_conversation,
        "unused",
        None,
        b"first raw",
        b"FIRST",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let second = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &prompts,
        },
        &first_conversation,
        "unused",
        None,
        b"second raw",
        b"SECOND",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    assert_eq!(
        second.submitted().receipt().previous_prompt_id(),
        Some(first.submitted().id())
    );

    let retry_one = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &prompts,
        },
        &first_conversation,
        "unused",
        None,
        b"retry one raw",
        b"RETRY ONE",
        &ModelInputOrigin::HarnessRetry {
            parent: Box::new(second.clone()),
        },
    )
    .unwrap();
    let retry_two = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &prompts,
        },
        &first_conversation,
        "unused",
        None,
        b"retry two raw",
        b"RETRY TWO",
        &ModelInputOrigin::HarnessRetry {
            parent: Box::new(retry_one.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        retry_two.submitted().receipt().previous_prompt_id(),
        Some(retry_one.submitted().id())
    );
    assert_eq!(
        retry_two.submitted().receipt().parent_prompt_id(),
        Some(retry_one.submitted().id())
    );
    assert_eq!(
        prompts
            .source(&first_conversation)
            .fetch_prompt(retry_one.submitted().id())
            .unwrap()
            .unwrap()
            .model_text(),
        b"RETRY ONE"
    );

    // `/new` changes this binding. Even though the process-local store
    // retains the old receipts until exit, the new view cannot address
    // them and therefore cannot leak the previous task.
    let new_conversation = newt_core::new_conversation_id();
    let fresh = begin_model_prompt(
        PromptIngress {
            durable: None,
            ephemeral: &prompts,
        },
        &new_conversation,
        "unused",
        None,
        b"fresh raw",
        b"FRESH",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let fresh_source = prompts.source(&new_conversation);
    assert!(fresh_source
        .fetch_prompt(retry_one.submitted().id())
        .unwrap()
        .is_none());
    assert!(fresh_source
        .fetch_prompt(fresh.submitted().id())
        .unwrap()
        .is_some());
}

#[test]
fn harness_retry_is_derived_and_keeps_operator_root_active() {
    let (_tmp, store, conversation_id) = prompt_store();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let operator = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "fix the parser",
        None,
        b"fix the parser",
        b"fix the parser",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let derived = ModelInputOrigin::HarnessRetry {
        parent: Box::new(operator.clone()),
    };
    let retry = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "fix the parser",
        None,
        b"/exit",
        b"/exit",
        &derived,
    )
    .unwrap();

    assert!(
        !derived.is_operator(),
        "retry must bypass human interceptors"
    );
    assert_eq!(
        retry.submitted_prompt().receipt().origin(),
        newt_core::PromptOrigin::HarnessRetry
    );
    assert_ne!(
        retry.submitted_prompt().id(),
        operator.submitted_prompt().id()
    );
    assert_eq!(
        retry.submitted_prompt().receipt().parent_prompt_id(),
        Some(operator.submitted_prompt().id())
    );
    assert_eq!(
        retry.active_operator_prompt().id(),
        operator.active_operator_prompt().id()
    );
    assert_eq!(
        retry.active_operator_prompt().model_text(),
        b"fix the parser"
    );

    let retry_again = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "fix the parser",
        None,
        b"ground it again",
        b"ground it again",
        &ModelInputOrigin::HarnessRetry {
            parent: Box::new(retry.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        retry_again.submitted_prompt().receipt().parent_prompt_id(),
        Some(retry.submitted_prompt().id()),
        "attempt ancestry must not be flattened to the operator root"
    );
    assert_eq!(
        retry_again.active_operator_prompt().id(),
        operator.active_operator_prompt().id()
    );
    assert_eq!(
        active_operator_task(Some(&retry_again), "ground it again"),
        "fix the parser",
        "derived memory must never relabel retry prose as the active task"
    );
}

#[test]
fn clarification_answer_is_an_operator_continuation_of_the_pending_objective() {
    let (_tmp, store, conversation_id) = prompt_store();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let original = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "implement the selected storage backend",
        None,
        b"implement either SQLite or Postgres",
        b"implement either SQLite or Postgres",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let answer_origin = ModelInputOrigin::OperatorContinuation {
        parent: Box::new(original.clone()),
    };
    let answer = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "implement the selected storage backend",
        None,
        b"1: SQLite",
        b"1: SQLite",
        &answer_origin,
    )
    .unwrap();

    assert!(answer_origin.is_operator());
    assert_eq!(
        answer.submitted_prompt().receipt().parent_prompt_id(),
        Some(original.submitted_prompt().id())
    );
    assert_eq!(
        answer.submitted_prompt().receipt().root_prompt_id(),
        original.submitted_prompt().receipt().root_prompt_id(),
        "a clarification answer must retain the original objective root"
    );
    // bug/steering-regressions (#1443): a clarification/decision CONTINUATION
    // refines the parent objective — it must NOT become the active operator
    // authority. Otherwise the protected prompt card carries the ceremony
    // reply ("1: SQLite") for the whole turn and mid-turn compaction can
    // evict the real task.
    assert_eq!(
        answer.active_operator_prompt().id(),
        original.submitted_prompt().id(),
        "a clarification answer must keep the original objective as active authority"
    );
    assert_ne!(
        answer.active_operator_prompt().id(),
        answer.submitted_prompt().id(),
        "the ceremony reply itself is not the active operator prompt"
    );
}

#[test]
fn durable_pending_clarification_rehydrates_its_lineage_after_resume() {
    let (_tmp, store, conversation_id) = prompt_store();
    let ephemeral_prompts = newt_core::agentic::SessionPromptStore::default();
    let original = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "select storage",
        None,
        b"Implement either SQLite or Postgres.",
        b"Implement either SQLite or Postgres.",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let unclear_answer = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "select storage",
        None,
        b"continue",
        b"continue",
        &ModelInputOrigin::OperatorContinuation {
            parent: Box::new(original.clone()),
        },
    )
    .unwrap();

    let restored = store
        .turn_prompt_context(&conversation_id, unclear_answer.submitted_prompt().id())
        .unwrap()
        .expect("durable prompt context");
    let pending = rehydrate_pending_clarification(&store, &conversation_id, &restored)
        .unwrap()
        .expect("unclear answer must remain an outstanding clarification");
    assert_eq!(
        pending.parent.submitted_prompt().id(),
        unclear_answer.submitted_prompt().id(),
        "the next answer must descend from the durable latest clarification receipt"
    );
    assert_eq!(
        pending.intake.disposition(),
        newt_core::agentic::PromptDisposition::Ask
    );

    let resolved = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral_prompts,
        },
        &conversation_id,
        "select storage",
        None,
        b"1: SQLite",
        b"1: SQLite",
        &ModelInputOrigin::OperatorContinuation {
            parent: pending.parent.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        resolved.submitted_prompt().receipt().root_prompt_id(),
        original.submitted_prompt().receipt().root_prompt_id(),
        "the resumed answer remains in the original objective"
    );
    let resolved_context = store
        .turn_prompt_context(&conversation_id, resolved.submitted_prompt().id())
        .unwrap()
        .expect("durable resolved context");
    assert!(
        rehydrate_pending_clarification(&store, &conversation_id, &resolved_context)
            .unwrap()
            .is_none(),
        "a fully explicit answer must clear the recovered pending state"
    );
}
