use super::*;

#[serial_test::serial(real_fs)]
#[test]
fn save_successful_turn_creates_and_reuses_active_conversation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(tmp.path(), workspace.path(), 100).unwrap();
    // The id is pre-assigned for the whole session (issue #220).
    let active_id = newt_core::new_conversation_id();
    let persona = Some(test_persona(
        "coder",
        "Code things.",
        tmp.path().join("personas").join("coder.md"),
    ));

    // First turn: no tool activity, backend reported usage (17.6).
    save_successful_conversation_turn(
        &store,
        &active_id,
        persona.as_ref(),
        "first task",
        "first reply",
        &[],
        &[],
        Some(newt_core::TokenUsage {
            input_tokens: 120,
            output_tokens: 45,
        }),
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();
    // Second turn: a recorded tool event, no usage (backend silent).
    let events = vec![newt_core::ToolEvent::from_call(
        "read_file",
        &serde_json::json!({"path": "src/lib.rs"}),
        true,
        Some(3),
    )];
    save_successful_conversation_turn(
        &store,
        &active_id,
        persona.as_ref(),
        "second task",
        "second reply",
        &events,
        &[],
        None,
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();

    let record = store.load(&active_id).unwrap();
    // First turn creates the record (title from the first task); the second
    // appends to the same id.
    assert_eq!(record.title, "first task");
    assert_eq!(record.persona.as_deref(), Some("coder"));
    assert_eq!(record.turns.len(), 2);
    // 17.6: token actuals and tool events ride the same save path.
    assert_eq!(record.turns[0].tokens_in, Some(120));
    assert_eq!(record.turns[0].tokens_out, Some(45));
    assert!(record.turns[0].events.is_empty());
    assert_eq!(record.turns[1].tokens_in, None, "no report → NULL, never 0");
    assert_eq!(record.turns[1].events, events);
}

#[test]
fn partial_ancillary_save_keeps_reply_durable_and_reports_its_true_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(tmp.path(), workspace.path(), 100).unwrap();
    let id = newt_core::new_conversation_id();

    let state = save_successful_conversation_turn_with_ancillary(
        &store,
        &id,
        None,
        "persist the reply",
        "reply is durable",
        &[],
        &[],
        None,
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
        |_, _, _, _| Err(anyhow::anyhow!("injected ancillary failure")),
    )
    .unwrap();

    assert!(matches!(
        state,
        TurnSaveState::DurableWithAncillaryWarning(_)
    ));
    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    assert_eq!(record.turns[0].assistant, "reply is durable");
}

/// #713: the per-turn save path threads the live scratchpad `<state>`
/// snapshot onto the conversation row, so `store.load()` reads it back —
/// the durable half of the resume fix (the restore half re-hydrates it).
#[test]
fn save_path_persists_scratchpad_snapshot() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(tmp.path(), workspace.path(), 100).unwrap();
    let active_id = newt_core::new_conversation_id();

    let mut state = std::collections::BTreeMap::new();
    state.insert("current_task".to_string(), "fix the parser".to_string());
    save_successful_conversation_turn(
        &store,
        &active_id,
        None,
        "do the task",
        "did it",
        &[],
        &[],
        None,
        None,
        &state,
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();

    let record = store.load(&active_id).unwrap();
    assert_eq!(
        record.scratchpad, state,
        "the live <state> snapshot must persist onto the conversation row"
    );
    // An empty snapshot on a later turn overwrites cleanly (latest wins).
    save_successful_conversation_turn(
        &store,
        &active_id,
        None,
        "clear it",
        "cleared",
        &[],
        &[],
        None,
        None,
        &std::collections::BTreeMap::new(),
        &newt_core::PlanSnapshot::default(),
    )
    .unwrap();
    assert!(
        store.load(&active_id).unwrap().scratchpad.is_empty(),
        "a later empty snapshot overwrites the saved <state>"
    );
}
