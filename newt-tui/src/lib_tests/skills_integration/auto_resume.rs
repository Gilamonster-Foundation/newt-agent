use super::*;

// -- 17.7: auto-resume, --ephemeral, NEWT_CONVERSATION_ID (#246) ---------

#[test]
fn session_start_precedence_chain() {
    // --ephemeral beats everything, including an explicit id and a name.
    assert_eq!(
        resolve_session_start(true, Some("some-id".into()), Some("a name".into()), true),
        SessionStart::Ephemeral
    );
    // NEWT_CONVERSATION_ID beats the name and the config key.
    assert_eq!(
        resolve_session_start(false, Some("some-id".into()), Some("a name".into()), true),
        SessionStart::ResumeExact("some-id".into())
    );
    assert_eq!(
        resolve_session_start(false, Some(" some-id ".into()), None, false),
        SessionStart::ResumeExact("some-id".into())
    );
    // #1671: --resume <name> beats the config key — on either setting.
    assert_eq!(
        resolve_session_start(false, None, Some("mesh docking".into()), true),
        SessionStart::ResumeNamed("mesh docking".into())
    );
    assert_eq!(
        resolve_session_start(false, None, Some(" mesh docking ".into()), false),
        SessionStart::ResumeNamed("mesh docking".into())
    );
    // A blank env var reads as unset, not as an impossible target.
    assert_eq!(
        resolve_session_start(false, Some("   ".into()), None, true),
        SessionStart::ResumeLatest
    );
    assert_eq!(
        resolve_session_start(false, None, Some("   ".into()), false),
        SessionStart::Fresh
    );
    // [conversations] resume decides the rest: on → latest, off → fresh.
    assert_eq!(
        resolve_session_start(false, None, None, true),
        SessionStart::ResumeLatest
    );
    assert_eq!(
        resolve_session_start(false, None, None, false),
        SessionStart::Fresh
    );
}

#[tokio::test]
async fn auto_resume_picks_latest_by_activity_tick_not_insertion_order() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let (_state, workspace, store, persona_store) = resume_fixture();
    // Two conversations; then the OLDER one gets a new turn, giving it
    // the highest §6 activity tick. Insertion order would pick `newer`;
    // the tick must pick `older`.
    let older = store.create("Older task", None).unwrap();
    store
        .append_turn(&older, "older question", "older answer")
        .unwrap();
    let newer = store.create("Newer task", None).unwrap();
    store
        .append_turn(&newer, "newer question", "newer answer")
        .unwrap();
    store
        .append_turn(&older, "older follow-up", "older again")
        .unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = rebuild_system_prompt(&workspace_str, &memory, None, "fresh-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    let banner = auto_resume_latest(&mut ctx).unwrap().expect("a banner");

    assert_eq!(
        active_conversation_id, older,
        "latest = highest activity tick, never insertion order"
    );
    assert!(
        banner.contains(short_conversation_id(&older)),
        "got: {banner}"
    );
    assert!(banner.contains("Older task"), "got: {banner}");
    assert!(banner.contains("(2 turns, last active ~"), "got: {banner}");
    assert!(banner.ends_with("— /new starts fresh"), "got: {banner}");
    // The resumed turns are the live session history now.
    let messages = memory.build_messages(&system, "next");
    assert!(messages.iter().any(|m| m.content == "older follow-up"));
    assert!(!messages.iter().any(|m| m.content == "newer question"));
}

#[test]
fn auto_resume_empty_workspace_is_silent_fresh_start() {
    let (_state, workspace, store, persona_store) = resume_fixture();
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = String::new();
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let fresh_id = active_conversation_id.clone();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    assert_eq!(auto_resume_latest(&mut ctx).unwrap(), None);
    assert_eq!(active_conversation_id, fresh_id, "fresh id untouched");
}

#[tokio::test]
async fn resume_exact_restores_that_conversation() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let (_state, workspace, store, persona_store) = resume_fixture();
    let target = store.create("Target work", None).unwrap();
    store
        .append_turn(&target, "target task", "target reply")
        .unwrap();
    // A more recently active conversation that exact-resume must ignore.
    let other = store.create("Other work", None).unwrap();
    store
        .append_turn(&other, "other task", "other reply")
        .unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = rebuild_system_prompt(&workspace_str, &memory, None, "fresh-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    let banner = resume_exact_conversation(&mut ctx, &target).unwrap();

    assert_eq!(active_conversation_id, target);
    assert!(banner.contains("Target work"), "got: {banner}");
    let messages = memory.build_messages(&system, "next");
    assert!(messages.iter().any(|m| m.content == "target task"));
    assert!(!messages.iter().any(|m| m.content == "other task"));
}

/// #713: resume re-hydrates the scratchpad `<state>` into the LIVE store, so
/// `state_get("current_task")` resolves on the first probe after an
/// interrupt instead of the round-0 black-hole "no such key". Restore is a
/// conversation boundary, so a stale live key from a prior conversation is
/// cleared and replaced by the resumed snapshot — never merged.
#[tokio::test]
async fn resume_rehydrates_scratchpad_state_into_live_store() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    use newt_core::ScratchpadStore;
    let (_state, workspace, store, persona_store) = resume_fixture();
    let id = store.create("Resume with state", None).unwrap();
    store.append_turn(&id, "set up state", "done").unwrap();
    // The model kept its task in <state>; persist that snapshot.
    let mut saved = std::collections::BTreeMap::new();
    saved.insert("current_task".to_string(), "fix the parser".to_string());
    saved.insert("open_file".to_string(), "src/parser.rs:128".to_string());
    store.update_scratchpad(&id, &saved).unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = rebuild_system_prompt(&workspace_str, &memory, None, "fresh-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    // A stale key from a "prior conversation" the boundary must drop.
    scratchpad_store.set("stale", "from before".to_string());
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    let banner = resume_exact_conversation(&mut ctx, &id).unwrap();

    // The exact round-0 probe now resolves from the live store.
    assert_eq!(
        scratchpad_store.get("current_task").as_deref(),
        Some("fix the parser"),
        "resumed <state> must land in the live store"
    );
    assert_eq!(
        scratchpad_store.get("open_file").as_deref(),
        Some("src/parser.rs:128")
    );
    // Boundary semantics: the stale key is gone, the snapshot is the whole map.
    assert_eq!(scratchpad_store.get("stale"), None, "restore clears first");
    assert_eq!(scratchpad_store.keys_count(), 2);
    // The banner tells the model its <state> came back so it does not blind-probe.
    assert!(
        banner.contains("— restored 2 <state> keys"),
        "got: {banner}"
    );
}

/// #715: resume re-hydrates the plan ledger into the LIVE ledger, so the
/// `<plan>` block / `plan_get` returns the saved plan — with the correct
/// active step and done statuses, NOT reset — instead of an empty plan after
/// an interrupt. Restore is a conversation boundary, so a stale live plan
/// from a prior conversation is cleared and replaced, never merged.
#[tokio::test]
async fn resume_rehydrates_plan_into_live_ledger() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    use newt_core::StepLedger;
    let (_state, workspace, store, persona_store) = resume_fixture();
    let id = store.create("Resume with plan", None).unwrap();
    store.append_turn(&id, "set up plan", "done").unwrap();
    // The model compiled a plan and advanced past step 1; persist that
    // ADVANCED snapshot (step 1 Done, step 2 Active, step 3 Todo).
    let source = newt_core::SessionStepLedger::default();
    source.set_plan(&[
        "read the code".to_string(),
        "write the fix".to_string(),
        "test it".to_string(),
    ]);
    source.advance();
    let saved = source.snapshot();
    store.update_plan_snapshot(&id, &saved).unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = rebuild_system_prompt(&workspace_str, &memory, None, "fresh-session");
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    // A stale plan from a "prior conversation" the boundary must drop.
    step_ledger.set_plan(&["stale step".to_string()]);
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    let banner = resume_exact_conversation(&mut ctx, &id).unwrap();

    // The resumed <plan> / plan_get now returns the saved plan verbatim —
    // boundary semantics: the stale step is gone, not merged.
    assert_eq!(step_ledger.snapshot(), saved, "resumed plan lands verbatim");
    assert_eq!(step_ledger.count(), 3);
    assert_eq!(step_ledger.done_count(), 1, "the Done step survives");
    let block = newt_core::plan_block(&step_ledger).expect("a non-empty <plan>");
    assert!(block.contains("✓ 1. read the code"), "{block}");
    assert!(block.contains("→ 2. write the fix"), "{block}");
    assert!(block.contains("☐ 3. test it"), "{block}");
    // The banner tells the model its plan came back so it does not re-plan.
    assert!(
        banner.contains("— restored plan (3 steps)"),
        "got: {banner}"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn resume_exact_errors_on_missing_and_foreign_workspace_ids() {
    let (state, workspace, store, persona_store) = resume_fixture();
    // A conversation that belongs to ANOTHER workspace on the same store
    // root — the 17.1b fence must keep it invisible here.
    let foreign_workspace = tempfile::TempDir::new().unwrap();
    let foreign_store =
        newt_core::ConversationStore::new(state.path(), foreign_workspace.path(), 100).unwrap();
    let foreign_id = foreign_store.create("Foreign work", None).unwrap();
    foreign_store
        .append_turn(&foreign_id, "theirs", "not ours")
        .unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(5));
    let workspace_str = workspace.path().to_str().unwrap().to_string();
    let mut system = String::new();
    let mut active_persona = None;
    let mut active_conversation_id = newt_core::new_conversation_id();
    let mut compress_state = newt_core::CompressState::new();
    let scratchpad_store = newt_core::SessionScratchpadStore::default();
    let step_ledger = newt_core::SessionStepLedger::default();
    let mut active_prompt_context = None;
    let mut ctx = ConversationCommandContext {
        store: &store,
        persona_store: &persona_store,
        workspace: &workspace_str,
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

    for id in [newt_core::new_conversation_id(), foreign_id] {
        let err = resume_exact_conversation(&mut ctx, &id).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("does not exist in this workspace"),
            "got: {msg}"
        );
        assert!(msg.contains("workspace fence"), "got: {msg}");
    }
    // Nothing leaked into the session from the failed resumes.
    let messages = memory.build_messages(&system, "next");
    assert!(!messages.iter().any(|m| m.content == "theirs"));
}

#[test]
fn auto_resume_banner_renders_claims_and_fresh_hint() {
    let mut record = newt_core::ConversationRecord {
        id: "1781136000000000000-abcd".into(),
        title: "Fix the parser".into(),
        workspace: "/ws".into(),
        workspace_id: "key".into(),
        persona: None,
        turns: Vec::new(),
        scratchpad: std::collections::BTreeMap::new(),
        plan: newt_core::PlanSnapshot::default(),
        roadmap_id: None,
        node_id: None,
        created_at_unix_nanos: 0,
        // 2026-06-11 00:00:00 UTC in nanos — must render ~-prefixed (§6:
        // a display claim, never the ordering key).
        updated_at_unix_nanos: 1_781_136_000 * 1_000_000_000,
    };
    let banner = auto_resume_banner(&record, "Fix the parser", None);
    assert_eq!(
        banner,
        "resumed conversation 178113600000  Fix the parser  \
             (0 turns, last active ~2026-06-11 00:00 UTC) — /new starts fresh"
    );
    // An empty scratchpad adds no note (the OFF/empty case stays silent).
    assert!(
        !banner.contains("<state>"),
        "empty scratchpad must not mention <state>: {banner}"
    );
    // A persona warning rides the banner rather than vanishing.
    let with_warning = auto_resume_banner(&record, "Fix the parser", Some("persona gone"));
    assert!(with_warning.ends_with("\nwarning: persona gone"));

    // #713: a restored scratchpad announces its key count so the model reads
    // its task instead of blind-probing `state_get("current_task")`.
    record
        .scratchpad
        .insert("current_task".into(), "fix the parser".into());
    let one = auto_resume_banner(&record, "Fix the parser", None);
    assert!(
        one.contains("— restored 1 <state> key") && !one.contains("<state> keys"),
        "singular key note: {one}"
    );
    // #718: a restored `current_task` is surfaced as an actionable pointer.
    assert!(
        one.contains("— last task: fix the parser"),
        "current_task value surfaced: {one}"
    );
    record
        .scratchpad
        .insert("open_file".into(), "src/parser.rs".into());
    let two = auto_resume_banner(&record, "Fix the parser", None);
    assert!(
        two.contains("— restored 2 <state> keys"),
        "plural key note: {two}"
    );
    assert!(
        two.contains("— last task: fix the parser"),
        "current_task still surfaced alongside other keys: {two}"
    );
    // The restored-keys note rides BEFORE any persona warning on its own line.
    let restored_with_warning = auto_resume_banner(&record, "Fix the parser", Some("persona gone"));
    assert!(
        restored_with_warning.contains("— restored 2 <state> keys")
            && restored_with_warning.ends_with("\nwarning: persona gone"),
        "got: {restored_with_warning}"
    );
    // #718: a long task value is capped (no unbounded banner).
    record
        .scratchpad
        .insert("current_task".into(), "x".repeat(200));
    let capped = auto_resume_banner(&record, "Fix the parser", None);
    assert!(
        capped.contains("— last task: "),
        "still has the pointer: {capped}"
    );
    assert!(capped.contains('…'), "long task value is elided: {capped}");
    // No `current_task` → no last-task pointer, just the key count.
    record.scratchpad.remove("current_task");
    let no_task = auto_resume_banner(&record, "Fix the parser", None);
    assert!(
        no_task.contains("— restored 1 <state> key") && !no_task.contains("last task:"),
        "no current_task → no last-task pointer: {no_task}"
    );

    // #715: an empty plan stays silent; a restored plan announces its step
    // count (singular / plural), so the model knows its <plan> came back.
    use newt_core::StepLedger;
    let empty_plan = newt_core::ConversationRecord {
        scratchpad: std::collections::BTreeMap::new(),
        plan: newt_core::PlanSnapshot::default(),
        ..record.clone()
    };
    assert!(
        !auto_resume_banner(&empty_plan, "Fix the parser", None).contains("restored plan"),
        "empty plan must not mention a restored plan"
    );
    let one_step = newt_core::SessionStepLedger::default();
    one_step.set_plan(&["only step".to_string()]);
    let mut with_plan = empty_plan.clone();
    with_plan.plan = one_step.snapshot();
    let one = auto_resume_banner(&with_plan, "Fix the parser", None);
    assert!(
        one.contains("— restored plan (1 step)") && !one.contains("(1 steps)"),
        "singular step note: {one}"
    );
    let three_steps = newt_core::SessionStepLedger::default();
    three_steps.set_plan(&["a".to_string(), "b".to_string(), "c".to_string()]);
    with_plan.plan = three_steps.snapshot();
    let three = auto_resume_banner(&with_plan, "Fix the parser", None);
    assert!(
        three.contains("— restored plan (3 steps)"),
        "plural step note: {three}"
    );
}
