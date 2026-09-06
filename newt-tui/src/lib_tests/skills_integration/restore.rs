use super::*;

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn conversation_restore_replaces_memory_and_restores_persona() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let state = tmp.path().join("state");
    let store = newt_core::ConversationStore::new(&state, &workspace, 100).unwrap();
    let id = store.create("Saved work", Some("reviewer")).unwrap();
    let saved_prompt = store
        .begin_prompt(
            &id,
            "Saved work",
            Some("reviewer"),
            newt_core::NewPrompt::operator(b"saved task".to_vec(), b"saved task".to_vec()),
        )
        .unwrap();
    store.append_turn(&id, "saved task", "saved reply").unwrap();

    let persona_dir = tmp.path().join("personas");
    fs::create_dir_all(&persona_dir).unwrap();
    fs::write(persona_dir.join("reviewer.md"), "Review from disk.").unwrap();
    let persona_store = PersonaStore::new(persona_dir);

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    memory
        .sync_all_with_active_task(
            "old task",
            "old reply",
            &newt_core::TurnMetrics::default(),
            "old task",
        )
        .await;
    let workspace_str = workspace.to_str().unwrap();
    let mut system = rebuild_system_prompt(workspace_str, &memory, None, "test-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    // A latched anti-thrash switch must be re-armed by restore too (F4):
    // restoring is a conversation boundary exactly like /new.
    let mut compress_state = newt_core::CompressState::new();
    compress_state.latch_disabled_for_tests();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mode_states = ConversationModeStates::default();
    let original_conversation_id = active_conversation_id.clone();
    let auto_control = mode_states.auto.bind(&original_conversation_id);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "admin")
        .unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();
    let mut conversation_ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: workspace_str,
        memory: &mut memory,
        system: &mut system,
        active_persona: &mut active_persona,
        active_conversation_id: &mut active_conversation_id,
        compress_state: &mut compress_state,
        scratchpad: &scratchpad_store,
        step_ledger: &step_ledger,
        active_prompt_context: &mut active_prompt_context,
        mode_states: &mode_states,
    };

    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let message = handle_conversation_command(
        &format!("/conversation restore {id}"),
        &mut conversation_ctx,
    )
    .unwrap();

    assert!(
        !conversation_ctx.compress_state.is_disabled(),
        "/conversation restore must reset compression anti-thrash (F4)"
    );
    assert!(message.contains("Restored conversation"));
    assert_eq!(*conversation_ctx.active_conversation_id, id);
    assert_eq!(
        mode_states.auto.pending_for(&original_conversation_id),
        None,
        "restore eagerly clears the outgoing conversation's Auto selection"
    );
    assert!(
        !mode_states.plan.is_active(),
        "restore clears the model-entered Plan phase"
    );
    assert_eq!(
        conversation_ctx
            .active_prompt_context
            .as_ref()
            .expect("restore rehydrates prompt metadata without executing it")
            .submitted_prompt()
            .id(),
        saved_prompt.submitted_prompt().id()
    );
    assert_eq!(
        conversation_ctx
            .active_persona
            .as_ref()
            .map(|p| p.name.as_str()),
        Some("reviewer")
    );
    assert!(conversation_ctx.system.contains("Review from disk."));
    let messages = conversation_ctx
        .memory
        .build_messages(conversation_ctx.system, "next task");
    assert!(!messages.iter().any(|m| m.content == "old task"));
    assert!(messages.iter().any(|m| m.content == "saved task"));
    assert!(messages.iter().any(|m| m.content == "saved reply"));

    let other = store.create("Other work", None).unwrap();
    let auto_control = mode_states.auto.bind(&id);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "admin")
        .unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();
    restore_conversation_into_session(&mut conversation_ctx, &other).unwrap();
    assert_eq!(mode_states.auto.pending_for(&id), None);
    assert!(!mode_states.plan.is_active());

    let auto_control = mode_states.auto.bind(&other);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "dev").unwrap();
    restore_conversation_into_session(&mut conversation_ctx, &id).unwrap();
    assert_eq!(
        mode_states.auto.pending_for(&other),
        None,
        "A→B→A restores cannot resurrect an Auto selection from either conversation"
    );
    assert_eq!(mode_states.auto.pending_for(&id), None);
    assert!(!mode_states.plan.is_active());
}

#[tokio::test]
async fn prompt_only_restore_rehydrates_receipt_without_replaying_it_as_input() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let state = tmp.path().join("state");
    let id = newt_core::new_conversation_id();
    let accepted_id = {
        let store = newt_core::ConversationStore::new(&state, &workspace, 100).unwrap();
        let accepted = store
            .begin_prompt(
                &id,
                "unfinished accepted prompt",
                None,
                newt_core::NewPrompt::operator(
                    b"unfinished accepted prompt".to_vec(),
                    b"unfinished accepted prompt".to_vec(),
                ),
            )
            .unwrap();
        assert!(store.load(&id).unwrap().turns.is_empty());
        accepted.submitted_prompt().id()
    };

    // A new store instance models a process restart. The receipt must rehydrate
    // as metadata, without appearing in presentation history or running itself.
    let store = newt_core::ConversationStore::new(&state, &workspace, 100).unwrap();

    let persona_store = PersonaStore::new(tmp.path().join("personas"));
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.to_str().unwrap();
    let mut system = rebuild_system_prompt(workspace_str, &memory, None, "fresh-session");
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
        memory: &mut memory,
        system: &mut system,
        active_persona: &mut active_persona,
        active_conversation_id: &mut active_conversation_id,
        compress_state: &mut compress_state,
        scratchpad: &scratchpad_store,
        step_ledger: &step_ledger,
        active_prompt_context: &mut active_prompt_context,
        mode_states: &ConversationModeStates::default(),
    };

    resume_exact_conversation(&mut ctx, &id).unwrap();

    assert_eq!(
        active_prompt_context
            .as_ref()
            .expect("prompt metadata rehydrated")
            .submitted_prompt()
            .id(),
        accepted_id
    );
    let messages = memory.build_messages(&system, "a new operator prompt");
    assert!(messages
        .iter()
        .any(|m| m.content == "a new operator prompt"));
    assert!(
        !messages
            .iter()
            .any(|m| m.content == "unfinished accepted prompt"),
        "restore must not replay or auto-execute a prompt-only receipt"
    );

    // Once the operator submits a new prompt, normal durable ingress creates a
    // chronological link to the restored prompt. The model-facing card/tool
    // tests in newt-core pin how that handle is surfaced and dereferenced.
    let continued = store
        .begin_prompt(
            &id,
            "ignored on an existing conversation",
            None,
            newt_core::NewPrompt::operator("continue", "continue"),
        )
        .unwrap();
    assert_eq!(
        continued.submitted().receipt().previous_prompt_id(),
        Some(accepted_id)
    );
    assert!(
        store.load(&id).unwrap().turns.is_empty(),
        "accepting the follow-up must not synthesize or replay a completed turn"
    );
}

/// #1668 review-2 finding 5: restoring a conversation re-seats the PERSONA
/// COGNITION LAYER, not merely the `active_persona` struct.
///
/// `handle_persona_command` always sets both, but `restore_conversation_into_session`
/// set only the struct. `effective_cognition` ranks the persona layer beneath the
/// CLI layer and above the default, so the outgoing conversation's persona
/// cognition stayed in force after switching to a conversation with a different
/// persona — or with none at all — while the banner named the incoming persona.
/// The operator saw one persona and ran another's dial.
///
/// Drives the real restore seam in all three directions, because the load-bearing
/// case is the CLEARING one: a stale layer is invisible precisely when the
/// incoming conversation declares nothing to overwrite it with.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn conversation_restore_reseats_the_persona_cognition_layer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let state = tmp.path().join("state");
    let store = newt_core::ConversationStore::new(&state, &workspace, 100).unwrap();

    // Two personas: one declaring a cognition dial, one declaring none.
    let persona_dir = tmp.path().join("personas");
    fs::create_dir_all(&persona_dir).unwrap();
    fs::write(
        persona_dir.join("thinker.md"),
        "+++\ncognition = \"contemplating\"\n+++\nThink hard.\n",
    )
    .unwrap();
    fs::write(persona_dir.join("plain.md"), "No front-matter here.\n").unwrap();
    let persona_store = PersonaStore::new(persona_dir);

    let thinking = store.create("Thinking work", Some("thinker")).unwrap();
    store.append_turn(&thinking, "q", "a").unwrap();
    let plain = store.create("Plain work", Some("plain")).unwrap();
    store.append_turn(&plain, "q", "a").unwrap();
    let personaless = store.create("No persona", None).unwrap();
    store.append_turn(&personaless, "q", "a").unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.to_str().unwrap();
    let mut system = rebuild_system_prompt(workspace_str, &memory, None, "test-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mode_states = ConversationModeStates::default();
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: workspace_str,
        memory: &mut memory,
        system: &mut system,
        active_persona: &mut active_persona,
        active_conversation_id: &mut active_conversation_id,
        compress_state: &mut compress_state,
        scratchpad: &scratchpad_store,
        step_ledger: &step_ledger,
        active_prompt_context: &mut active_prompt_context,
        mode_states: &mode_states,
    };

    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    newt_core::cognition::set_persona_cognition(None);

    // 1. Restoring a persona that declares a dial SEATS it.
    restore_conversation_into_session(&mut ctx, &thinking).unwrap();
    assert_eq!(
        newt_core::cognition::persona_cognition(),
        Some(newt_core::role_profile::Cognition::Contemplating),
        "restoring a conversation whose persona declares cognition seats that layer"
    );

    // 2. Restoring a persona that declares NONE clears it. This is the bug:
    //    the struct swapped to `plain` while the layer stayed contemplating.
    restore_conversation_into_session(&mut ctx, &plain).unwrap();
    assert_eq!(
        ctx.active_persona.as_ref().map(|p| p.name.as_str()),
        Some("plain")
    );
    assert_eq!(
        newt_core::cognition::persona_cognition(),
        None,
        "a persona declaring no cognition must not inherit the outgoing persona's dial"
    );

    // 3. And restoring a conversation with NO persona clears it too.
    restore_conversation_into_session(&mut ctx, &thinking).unwrap();
    assert_eq!(
        newt_core::cognition::persona_cognition(),
        Some(newt_core::role_profile::Cognition::Contemplating)
    );
    restore_conversation_into_session(&mut ctx, &personaless).unwrap();
    assert!(ctx.active_persona.is_none());
    assert_eq!(
        newt_core::cognition::persona_cognition(),
        None,
        "a conversation with no persona runs with no persona dial"
    );
}
