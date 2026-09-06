use super::*;

// =========================================================================
// Part 5 — 17.6: tool-event + token-usage recording (issue #246).
// The turn grows past `(task, reply)`: `append_turn_full` persists the
// loop's recorded ToolEvents into the `events` JSON column and the
// backend-reported token actuals into `tokens_in`/`tokens_out`. Events are
// §6 content (chain-covered), their digests never carry raw args, and the
// 17.3 FTS trigger picks the new columns up with no schema work.
// =========================================================================

#[test]
fn tool_events_and_tokens_round_trip_through_append_and_load() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("tooling", None).unwrap();
    let events = sample_events();
    store
        .append_turn_full(
            &id,
            "fix the bug",
            "fixed",
            &events,
            &[],
            &[],
            Some(1_204),
            Some(892),
        )
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    let turn = &record.turns[0];
    assert_eq!(turn.user, "fix the bug");
    assert_eq!(turn.assistant, "fixed");
    assert_eq!(turn.events, events, "events must round-trip verbatim");
    assert_eq!(turn.tokens_in, Some(1_204));
    assert_eq!(turn.tokens_out, Some(892));
    // The outcome and duration claims survive too.
    assert!(turn.events[0].ok);
    assert!(!turn.events[1].ok);
    assert_eq!(turn.events[1].duration_ms, Some(2_500));
}

/// #717: the per-turn phantom-reach telemetry persists into its own
/// `phantom_reaches` column and reloads verbatim — distinct from `events`.
/// Also proves the §6 content chain still verifies, i.e. the new column is
/// additive and NOT folded into the canonical encoding (telemetry, not
/// provenance), so existing chains remain valid byte-for-byte.
#[test]
fn phantom_reaches_round_trip_and_chain_still_verifies() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("phantoms", None).unwrap();
    let phantoms = vec![
        PhantomReach {
            name_as_called: "bash".to_string(),
            resolution: PhantomResolution::Rewrite("run_command".to_string()),
            active_context_features: Vec::new(),
        },
        PhantomReach {
            name_as_called: "enter_plan_mode".to_string(),
            resolution: PhantomResolution::Unknown,
            active_context_features: Vec::new(),
        },
    ];
    store
        .append_turn_full(&id, "do it", "done", &[], &phantoms, &[], None, None)
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    let turn = &record.turns[0];
    assert_eq!(
        turn.phantom_reaches, phantoms,
        "phantom reaches must round-trip verbatim"
    );
    // They are distinct telemetry: no tool events were recorded this turn.
    assert!(
        turn.events.is_empty(),
        "phantom reaches are not tool events"
    );
    // The new column rides outside the §6 canonical encoding, so the chain
    // — populated with a non-empty phantom payload — still verifies.
    store.verify_chain(&id).unwrap();
}

/// #713: the conversation scratchpad `<state>` snapshot persists into its own
/// `scratchpad` column and reloads verbatim, so an interrupt + auto-resume can
/// re-hydrate the live store. Also proves the §6 content chain still verifies,
/// i.e. the column is additive and NOT folded into the canonical encoding
/// (working memory, not provenance) — existing chains remain valid
/// byte-for-byte.
#[test]
fn scratchpad_round_trips_and_chain_still_verifies() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("scratchpad", None).unwrap();
    // A turn establishes the §6 chain we will re-verify after the scratchpad
    // write — proving the scratchpad rides outside the chain.
    store
        .append_turn(&id, "what were we doing?", "fixing the parser")
        .unwrap();
    let tip_before = store.load(&id).unwrap();
    assert!(
        tip_before.scratchpad.is_empty(),
        "a fresh row carries the empty `{{}}` backfill"
    );

    let mut state = std::collections::BTreeMap::new();
    state.insert("current_task".to_string(), "fix the parser".to_string());
    state.insert("open_file".to_string(), "src/parser.rs:128".to_string());
    store.update_scratchpad(&id, &state).unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(
        record.scratchpad, state,
        "scratchpad <state> must round-trip verbatim through save + load"
    );
    // The exact round-0 black-hole probe now resolves from the restored snapshot.
    assert_eq!(
        record.scratchpad.get("current_task").map(String::as_str),
        Some("fix the parser"),
        "the resumed `state_get(\"current_task\")` survives"
    );
    // The scratchpad rides the conversation row, outside the §6 canonical
    // encoding, so the chain — written before AND independent of the scratchpad
    // — still verifies byte-for-byte.
    store.verify_chain(&id).unwrap();

    // An overwrite (the live store mutating across turns) replaces, not merges.
    let mut state2 = std::collections::BTreeMap::new();
    state2.insert("current_task".to_string(), "ship the fix".to_string());
    store.update_scratchpad(&id, &state2).unwrap();
    let reloaded = store.load(&id).unwrap();
    assert_eq!(reloaded.scratchpad, state2, "latest snapshot wins");
    assert!(
        !reloaded.scratchpad.contains_key("open_file"),
        "a fresh snapshot is the whole map, not a merge"
    );
    store.verify_chain(&id).unwrap();
}

/// #715: the conversation plan-ledger snapshot persists into its own `plan`
/// column and reloads VERBATIM — including which steps are Done and which is
/// Active (the full state `set_plan` would reset), so an interrupt + auto-resume
/// can re-hydrate the live ledger. Also proves the §6 content chain still
/// verifies, i.e. the column is additive and NOT folded into the canonical
/// encoding (working memory, not provenance).
#[test]
fn plan_snapshot_round_trips_and_chain_still_verifies() {
    use newt_core::StepLedger;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("plan", None).unwrap();
    // A turn establishes the §6 chain we re-verify after the plan write.
    store
        .append_turn(&id, "what is the plan?", "let me set one")
        .unwrap();
    let before = store.load(&id).unwrap();
    assert!(
        before.plan.is_empty(),
        "a fresh row carries the empty `{{}}` backfill"
    );

    // Build an ADVANCED ledger: step 1 Done, step 2 Active, step 3 Todo.
    let ledger = newt_core::SessionStepLedger::default();
    ledger.set_plan(&[
        "read the code".to_string(),
        "write the fix".to_string(),
        "test it".to_string(),
    ]);
    ledger.advance(); // step 1 → Done, step 2 → Active
    let snap = ledger.snapshot();
    store.update_plan_snapshot(&id, &snap).unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(
        record.plan, snap,
        "plan snapshot must round-trip verbatim (steps + statuses)"
    );
    // The active step + done statuses survive — not reset to a fresh plan.
    assert_eq!(record.plan.len(), 3);
    assert_eq!(record.plan.steps[0].status, newt_core::StepStatus::Done);
    assert_eq!(record.plan.steps[1].status, newt_core::StepStatus::Active);
    assert_eq!(record.plan.steps[2].status, newt_core::StepStatus::Todo);
    // The plan rides the conversation row, outside the §6 canonical encoding, so
    // the chain — written before AND independent of the plan — still verifies.
    store.verify_chain(&id).unwrap();

    // A later snapshot replaces, not merges (the live ledger advancing a step).
    ledger.advance(); // step 2 → Done, step 3 → Active
    store.update_plan_snapshot(&id, &ledger.snapshot()).unwrap();
    let reloaded = store.load(&id).unwrap();
    assert_eq!(reloaded.plan.steps[1].status, newt_core::StepStatus::Done);
    assert_eq!(reloaded.plan.steps[2].status, newt_core::StepStatus::Active);
    store.verify_chain(&id).unwrap();
}

/// The args digest is keys + hash, never values: feed a secret-looking arg
/// and prove the stored row carries no trace of it anywhere.
#[test]
fn args_digest_never_carries_raw_arg_values() {
    let secret = "AKIA-hunter2-SUPERSECRET";
    let event = ToolEvent::from_call(
        "write_file",
        &serde_json::json!({"path": "creds.env", "content": secret}),
        true,
        None,
    );
    // Key names are searchable; the value is absent (only a digest remains).
    assert!(event.args_digest.contains("content"));
    assert!(event.args_digest.contains("path"));
    assert!(event.args_digest.contains("b3:"));
    assert!(
        !event.args_digest.contains("hunter2"),
        "raw arg values must never reach the digest: {}",
        event.args_digest
    );
    assert!(!event.args_digest.contains("creds.env"));

    // End to end: nothing in the persisted row leaks the secret either.
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("secret turn", None).unwrap();
    store
        .append_turn_full(&id, "write creds", "done", &[event], &[], &[], None, None)
        .unwrap();
    let stored: String = raw(root.path())
        .query_row(
            "SELECT events FROM turns WHERE conversation_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains("hunter2"), "leaked into events: {stored}");
    // And identical args correlate: same digest, different turn.
    let again = ToolEvent::from_call(
        "write_file",
        &serde_json::json!({"path": "creds.env", "content": secret}),
        true,
        None,
    );
    assert_eq!(
        again.args_digest,
        store.load(&id).unwrap().turns[0].events[0].args_digest
    );
}

/// Tokens are measurements: present when the backend reported them, NULL
/// when it did not — never a zero or an estimate dressed as one (18.5
/// rehydrates from these columns and must be able to trust them).
#[test]
fn absent_backend_usage_stores_null_not_a_guess() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("usage", None).unwrap();

    store
        .append_turn_full(&id, "with usage", "ok", &[], &[], &[], Some(100), Some(20))
        .unwrap();
    store
        .append_turn_full(&id, "backend silent", "ok", &[], &[], &[], None, None)
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns[0].tokens_in, Some(100));
    assert_eq!(record.turns[0].tokens_out, Some(20));
    assert_eq!(record.turns[1].tokens_in, None);
    assert_eq!(record.turns[1].tokens_out, None);

    // At the SQL level the silent turn is genuinely NULL, not 0.
    let (tin, tout): (Option<i64>, Option<i64>) = raw(root.path())
        .query_row(
            "SELECT tokens_in, tokens_out FROM turns
              WHERE conversation_id = ?1 AND user = 'backend silent'",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((tin, tout), (None, None));
}

/// The 17.3 AFTER INSERT trigger derives tool_names/tool_args_digest from
/// the events JSON — so a turn appended through `append_turn_full` is
/// immediately recallable by the tool name it used and by digest terms.
#[test]
fn fts_finds_tool_names_recorded_by_append_turn_full() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("deploy day", None).unwrap();
    store
        .append_turn_full(
            &id,
            "ship it",
            "shipped",
            &[ToolEvent::from_call(
                "web_fetch",
                &serde_json::json!({"url": "https://release.example"}),
                true,
                Some(90),
            )],
            &[],
            &[],
            Some(50),
            Some(10),
        )
        .unwrap();

    // Tool name hits via the derived tool_names column…
    let hits = store.search("web_fetch", 10).unwrap();
    assert_eq!(hits.len(), 1, "a recorded tool name must be recallable");
    assert_eq!(hits[0].conversation_id, id);
    assert!(hits[0].snippet.contains(">>>"), "{}", hits[0].snippet);
    // …and digest key terms via tool_args_digest ("url" is in the digest;
    // the URL value itself never reached the index).
    assert_eq!(store.search("url", 10).unwrap().len(), 1);
    assert!(store.search("release.example", 10).unwrap().is_empty());
}

/// §6: events and token counts are row content. The chain verifies with
/// them populated, and editing a stored event after the fact breaks it —
/// same tamper evidence as user/assistant text.
#[test]
fn chain_verifies_with_events_and_detects_event_tampering() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("chained tools", None).unwrap();

    // Mixed history: plain turn, evented turn, plain turn.
    store.append_turn(&id, "plan", "planned").unwrap();
    store
        .append_turn_full(
            &id,
            "act",
            "acted",
            &sample_events(),
            &[],
            &[],
            Some(700),
            None,
        )
        .unwrap();
    store.append_turn(&id, "wrap", "wrapped").unwrap();
    store
        .verify_chain(&id)
        .expect("populated events must verify under the unchanged v1 encoding");

    // Rewriting history's tool record is detectable.
    let changed = raw(root.path())
        .execute(
            "UPDATE turns SET events = '[]' WHERE conversation_id = ?1 AND user = 'act'",
            [&id],
        )
        .unwrap();
    assert_eq!(changed, 1);
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation"),
        "tampered events must break the chain: {err}"
    );
}

/// Back-compat: rows written before 17.6 (events = '[]', token columns
/// NULL — exactly what plain `append_turn` still writes) load as empty
/// events and absent tokens, and verify unchanged.
#[test]
fn pre_17_6_rows_with_empty_events_still_load_and_verify() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("legacy shape", None).unwrap();
    store.append_turn(&id, "old task", "old reply").unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    assert!(record.turns[0].events.is_empty());
    assert_eq!(record.turns[0].tokens_in, None);
    assert_eq!(record.turns[0].tokens_out, None);
    // The wrapper writes the byte-identical pre-17.6 shape ('[]'/NULL)…
    let stored: (String, Option<i64>) = raw(root.path())
        .query_row(
            "SELECT events, tokens_in FROM turns WHERE conversation_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("[]".to_string(), None));
    // …so pre-17.6 chains keep verifying under the same v1 encoding.
    store.verify_chain(&id).unwrap();

    // A garbage events blob (writable only by an external tool) refuses to
    // load as silent garbage — the encoding_version philosophy.
    raw(root.path())
        .execute(
            "UPDATE turns SET events = 'not json' WHERE conversation_id = ?1",
            [&id],
        )
        .unwrap();
    let err = store.load(&id).unwrap_err().to_string();
    assert!(err.contains("tool-event"), "{err}");
}
