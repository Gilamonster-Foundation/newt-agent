use super::*;

#[test]
fn ephemeral_session_saves_nothing() {
    let (_state, _workspace, store, _persona_store) = resume_fixture();
    // The ephemeral arm of the save seam: no store handle → no row, no
    // turn, no error — asserted against a real store on the same root.
    save_turn_if_persistent(
        None,
        &newt_core::new_conversation_id(),
        None,
        "ephemeral task",
        "ephemeral reply",
        &[],
        &[],
        None,
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();
    assert!(
        store.list().unwrap().is_empty(),
        "--ephemeral must leave zero conversation rows"
    );
    // The persistent arm still writes (the seam routes, never drops).
    let id = newt_core::new_conversation_id();
    save_turn_if_persistent(
        Some(&store),
        &id,
        None,
        "kept task",
        "kept reply",
        &[],
        &[],
        None,
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn ephemeral_notice_names_both_halves() {
    // The notice doubles as the /conversation + /recall answer in an
    // ephemeral session: it must say nothing is saved AND nothing resumed.
    assert!(EPHEMERAL_SESSION_NOTICE.contains("nothing saved"));
    assert!(EPHEMERAL_SESSION_NOTICE.contains("nothing resumed"));
}

/// Step 18.5 (#247) compressed-session round-trip: a session that
/// compressed mid-flight persists the compaction record through the save
/// path; a fresh session restoring it gets the summary message back in
/// the working set (recognizable by the pipeline's marker) instead of
/// the raw pre-compression history — the memory.rs:919-class bug.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn compressed_session_round_trips_summary_through_save_and_restore() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let store =
        newt_core::ConversationStore::new(tmp.path().join("state"), &workspace, 100).unwrap();
    let id = newt_core::new_conversation_id();

    let metrics = |input_tokens: u32| newt_core::TurnMetrics {
        usage: Some(newt_core::TokenUsage {
            input_tokens,
            output_tokens: 9,
        }),
        ..Default::default()
    };

    // Live session: Summarizing provider with a stub summarizer.
    let mut memory = newt_core::MemoryManager::new();
    // Leave enough room for the irreducible active-prompt metadata + exact
    // user pair. A smaller authoritative budget must refuse compression
    // rather than summarize either half of that pair.
    memory.add_provider(newt_core::Summarizing::new(512).with_summarizer(
        |_req: String| -> newt_core::SummarizeFuture {
            Box::pin(async { Ok("FACTS FROM THE COMPRESSED MIDDLE".to_string()) })
        },
    ));
    let big = "x".repeat(200);
    for i in 0..5u32 {
        let task = format!("early task {i}");
        memory
            .sync_all_with_active_task(&task, &big, &metrics(10 + i), &task)
            .await;
        save_successful_conversation_turn(
            &store,
            &id,
            None,
            &task,
            &big,
            &[],
            &[],
            Some(newt_core::TokenUsage {
                input_tokens: 10 + i,
                output_tokens: 9,
            }),
            memory.take_compaction_record(),
            &std::collections::BTreeMap::new(),
            &newt_core::PlanSnapshot::default(),
        )
        .unwrap();
    }
    // The over-budget turn mints the compaction record during sync.
    memory
        .sync_all_with_active_task("final task", &big, &metrics(600), "final task")
        .await;
    let record = memory.take_compaction_record();
    assert!(record.is_some(), "compression must mint a record");
    save_successful_conversation_turn(
        &store,
        &id,
        None,
        "final task",
        &big,
        &[],
        &[],
        Some(newt_core::TokenUsage {
            input_tokens: 600,
            output_tokens: 9,
        }),
        record,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();

    // Fresh session restores through the command path (no summarizer —
    // restore must never need one).
    let persona_store = PersonaStore::new(tmp.path().join("personas"));
    let mut memory2 = newt_core::MemoryManager::new();
    memory2.add_provider(newt_core::Summarizing::new(512));
    let workspace_str = workspace.to_str().unwrap();
    let mut system = rebuild_system_prompt(workspace_str, &memory2, None, "test-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: workspace_str,
        memory: &mut memory2,
        system: &mut system,
        active_persona: &mut active_persona,
        active_conversation_id: &mut active_conversation_id,
        compress_state: &mut compress_state,
        scratchpad: &scratchpad_store,
        step_ledger: &step_ledger,
        active_prompt_context: &mut active_prompt_context,
        mode_states: &ConversationModeStates::default(),
    };
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    handle_conversation_command(&format!("/conversation restore {id}"), &mut ctx).unwrap();

    let messages = memory2.build_messages(&system, "next task");
    let summary = messages
        .iter()
        .find(|m| m.content.starts_with(newt_core::agentic::SUMMARY_PREFIX))
        .expect("the compaction summary must survive restore");
    assert!(summary.content.contains("FACTS FROM THE COMPRESSED MIDDLE"));
    assert!(summary
        .content
        .contains(newt_core::agentic::SUMMARY_END_MARKER));
    // The triggering turn survives alongside the summary; the summarized
    // early history is not duplicated next to its own summary.
    assert!(messages.iter().any(|m| m.content == "final task"));
    assert!(!messages.iter().any(|m| m.content == "early task 0"));
    // The lone-sided summary record never dispatches an empty message.
    assert!(!messages.iter().any(|m| m.content.is_empty()));
}
