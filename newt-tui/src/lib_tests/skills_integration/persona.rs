use super::*;

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_includes_active_persona_overlay() {
    let tmp = tempfile::TempDir::new().unwrap();
    let persona = test_persona(
        "reviewer",
        "Review from a persona file.",
        tmp.path().join("personas").join("reviewer.md"),
    );
    let prompt = build_system_prompt_with_persona(
        tmp.path().to_str().unwrap(),
        Some(newt_core::DEFAULT_SOUL),
        Some(&persona),
        "test-plan.md",
    );
    assert!(prompt.contains("Active persona: reviewer"));
    assert!(prompt.contains("Review from a persona file."));
}

/// FR-5 (#999): a persona whose front-matter declares `altitude = "coach"`
/// REPLACES the base identity with COACH_SOUL instead of layering a coach
/// overlay onto the doer DEFAULT_SOUL (which would ship two contradictory
/// identities in one prompt). A persona with no altitude keeps the doer soul.
#[serial_test::serial(real_fs)]
#[test]
fn coach_altitude_replaces_identity_with_coach_soul() {
    let tmp = tempfile::TempDir::new().unwrap();
    let profile =
        newt_core::RoleProfile::parse("+++\naltitude = \"coach\"\n+++\n\nBe a patient reviewer.\n")
            .unwrap();
    let coach = Persona {
        name: "coach".to_string(),
        prompt: profile.prompt.clone(),
        path: tmp.path().join("coach.md"),
        profile,
    };
    let prompt = build_system_prompt_with_persona(
        tmp.path().to_str().unwrap(),
        Some(newt_core::DEFAULT_SOUL),
        Some(&coach),
        "test-plan.md",
    );
    assert!(
        prompt.contains("COACH mode"),
        "coach altitude installs COACH_SOUL"
    );
    assert!(
        !prompt.contains("On an `act` turn, never describe a code change"),
        "coach altitude REPLACES the doer soul, it does not append to it"
    );
    assert!(
        prompt.contains("Be a patient reviewer."),
        "the persona's own overlay still rides on top"
    );

    // No altitude → the doer identity is unchanged.
    let doer = test_persona("worker", "Do the work.", tmp.path().join("worker.md"));
    let doer_prompt = build_system_prompt_with_persona(
        tmp.path().to_str().unwrap(),
        Some(newt_core::DEFAULT_SOUL),
        Some(&doer),
        "test-plan.md",
    );
    assert!(doer_prompt.contains("On an `act` turn, never describe a code change"));
    assert!(!doer_prompt.contains("COACH mode"));
}

#[test]
fn persona_commands_parse_expected_actions() {
    assert_eq!(
        parse_persona_command("/persona reviewer").unwrap(),
        PersonaCommand::set("reviewer")
    );
    assert_eq!(
        parse_persona_command("/persona set security").unwrap(),
        PersonaCommand::set("security")
    );
    // FR-PA-1 (#1021): `switch` is a discoverable alias for `set`.
    assert_eq!(
        parse_persona_command("/persona switch personal-assistant").unwrap(),
        PersonaCommand::set("personal-assistant")
    );
    assert_eq!(
        parse_persona_command("/persona clear").unwrap(),
        PersonaCommand::Clear
    );
    assert_eq!(
        parse_persona_command("/persona show").unwrap(),
        PersonaCommand::Show
    );
    assert_eq!(
        parse_persona_command("/persona list").unwrap(),
        PersonaCommand::List
    );
    assert_eq!(
        parse_persona_command("/persona default").unwrap(),
        PersonaCommand::set("coder")
    );
}

#[test]
fn persona_set_parses_keep_context_flag() {
    // `--keep-context` flips keep_context regardless of position.
    assert_eq!(
        parse_persona_command("/persona set worker --keep-context").unwrap(),
        PersonaCommand::Set {
            name: "worker".into(),
            keep_context: true,
        }
    );
    assert_eq!(
        parse_persona_command("/persona --keep-context set worker").unwrap(),
        PersonaCommand::Set {
            name: "worker".into(),
            keep_context: true,
        }
    );
    // Default (no flag) keeps the reset-on-swap behavior.
    assert_eq!(
        parse_persona_command("/persona set worker").unwrap(),
        PersonaCommand::Set {
            name: "worker".into(),
            keep_context: false,
        }
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn persona_store_writes_coder_default_only_when_loaded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    let store = PersonaStore::new(dir.clone());

    assert!(
        !dir.exists(),
        "constructing a store must not write defaults"
    );

    let persona = store.load("coder").unwrap();

    assert_eq!(persona.name, "coder");
    assert_eq!(persona.path, dir.join("coder.md"));
    assert!(persona.prompt.contains(newt_core::DEFAULT_SOUL));
    assert!(
        dir.join("coder.md").is_file(),
        "first persona load should materialize the default coder file"
    );
}

/// FR-16 (#1000): per-file idempotent seeding — each MISSING shipped default
/// is written beside the user's own personas (so an upgrade receives a
/// newly-added default like `coach`), while an existing default the user has
/// edited is left untouched. Supersedes the old empty-dir-only contract.
#[serial_test::serial(real_fs)]
#[test]
fn persona_store_seeds_missing_defaults_without_clobbering() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    fs::create_dir_all(&dir).unwrap();
    // A user's own persona AND a hand-edited coder are present; only the
    // coach default is missing (the pre-FR-16 upgrader's exact situation).
    fs::write(dir.join("reviewer.md"), "Review from disk.").unwrap();
    fs::write(dir.join("coder.md"), "MY custom coder").unwrap();
    let store = PersonaStore::new(dir.clone());

    let names: std::collections::HashSet<String> =
        store.list().unwrap().into_iter().map(|p| p.name).collect();

    assert!(
        names.contains("coach"),
        "missing coach default must be seeded"
    );
    assert!(names.contains("reviewer"), "user persona preserved");
    assert!(names.contains("coder"), "edited coder preserved");
    assert_eq!(
        fs::read_to_string(dir.join("coder.md")).unwrap(),
        "MY custom coder",
        "an existing default must NOT be overwritten"
    );
}

/// FR-16 (#1000): the seeded coach is a read-only, coach-altitude persona —
/// FR-5 swaps its identity to COACH_SOUL, FR-1 enforces its caveats, and its
/// tool allow-list grants no mutating tool.
#[serial_test::serial(real_fs)]
#[test]
fn seeded_coach_is_a_read_only_coach_altitude_persona() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
    let coach = store.load("coach").unwrap();
    assert_eq!(coach.profile.altitude, Some(newt_core::Altitude::Coach));
    assert!(coach.profile.caveats.is_some(), "coach declares [caveats]");
    let tools = coach
        .profile
        .tools
        .expect("coach declares a tools allow-list");
    for banned in ["write_file", "edit_file", "run_command"] {
        assert!(
            !tools.contains(&banned.to_string()),
            "coach must not grant `{banned}`"
        );
    }
}

/// #1021 FR-PA-3: `personal-assistant` seeds as the third default
/// persona, per-file-idempotently — same seeding mechanism FR-16 proved
/// for `coach` above, extended to the newest default.
#[serial_test::serial(real_fs)]
#[test]
fn seeded_personal_assistant_binds_gila_skill() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
    let names: Vec<String> = store.list().unwrap().into_iter().map(|p| p.name).collect();
    assert!(
        names.contains(&"personal-assistant".to_string()),
        "seeded alongside coder/coach: {names:?}"
    );
    let pa = store.load("personal-assistant").unwrap();
    assert_eq!(
        pa.profile.skills,
        Some(vec!["gila-personal-assistant".to_string()])
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn persona_set_starts_fresh_conversation_with_overlay() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_str().unwrap();
    let persona_dir = tmp.path().join("personas");
    fs::create_dir_all(&persona_dir).unwrap();
    fs::write(persona_dir.join("reviewer.md"), "Review from disk.").unwrap();
    let store = PersonaStore::new(persona_dir);
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
    let mut system = rebuild_system_prompt(workspace, &memory, None, "test-session");
    let mut active_persona = None;
    let mut active_conversation_id = String::from("test-session");
    let mode_states = ConversationModeStates::default();
    let auto_control = mode_states.auto.bind(&active_conversation_id);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "admin")
        .unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();

    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let message = {
        let mut ctx = ConversationResetContext {
            memory: &mut memory,
            system: &mut system,
            conversation_id: &mut active_conversation_id,
            mode_states: &mode_states,
        };
        handle_persona_command(
            "/persona reviewer",
            workspace,
            &store,
            &mut active_persona,
            &mut ctx,
        )
        .unwrap()
    };

    assert_eq!(
        message,
        "Started a new conversation with persona `reviewer`."
    );
    assert_eq!(
        active_persona.as_ref().map(|p| p.name.as_str()),
        Some("reviewer")
    );
    assert!(system.contains("Active persona: reviewer"));
    assert!(system.contains("Review from disk."));
    let messages = memory.build_messages(&system, "new task");
    assert!(!messages.iter().any(|m| m.content == "old task"));
    assert!(!messages.iter().any(|m| m.content == "old reply"));
    assert_eq!(
        mode_states.auto.pending_for("test-session"),
        None,
        "persona-created conversations clear pending Auto state"
    );
    assert!(
        !mode_states.plan.is_active(),
        "persona-created conversations clear model-entered Plan"
    );
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn new_conversation_preserves_active_persona() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_str().unwrap();
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
    let active_persona = Some(test_persona(
        "terse",
        "Keep replies short.",
        tmp.path().join("personas").join("terse.md"),
    ));
    let mut system =
        rebuild_system_prompt(workspace, &memory, active_persona.as_ref(), "test-session");
    let mut active_conversation_id = String::from("test-session");

    // A latched anti-thrash switch must be re-armed by /new (F4): the
    // disable notice promises "start a new conversation to reset".
    let mut compress_state = newt_core::CompressState::new();
    compress_state.latch_disabled_for_tests();

    let mut session_opted_fresh = false;
    let mode_states = ConversationModeStates::default();
    let auto_control = mode_states.auto.bind(&active_conversation_id);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "admin")
        .unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();
    let mut ctx = ConversationResetContext {
        memory: &mut memory,
        system: &mut system,
        conversation_id: &mut active_conversation_id,
        mode_states: &mode_states,
    };
    let scratchpad = newt_core::SessionScratchpadStore::default();
    let ledger = newt_core::SessionStepLedger::default();
    let mut prompt_ctx = None;
    let message = handle_new_conversation(
        workspace,
        active_persona.as_ref(),
        &mut ctx,
        &mut compress_state,
        &mut session_opted_fresh,
        &mut ConversationScopedState {
            scratchpad: &scratchpad,
            step_ledger: &ledger,
            active_prompt_context: &mut prompt_ctx,
        },
    );

    assert!(
        !compress_state.is_disabled(),
        "/new must reset compression anti-thrash (F4)"
    );
    // 17.7: /new opts the session out of auto-resume — for good.
    assert!(session_opted_fresh, "/new must set the session fresh flag");
    assert!(
        !should_auto_resume(&SessionStart::ResumeLatest, session_opted_fresh),
        "auto-resume must never undo an explicit /new"
    );
    assert_eq!(message, "Started a new conversation with persona `terse`.");
    assert!(system.contains("Active persona: terse"));
    assert!(system.contains("Keep replies short."));
    let messages = memory.build_messages(&system, "new task");
    assert!(!messages.iter().any(|m| m.content == "old task"));
    assert!(!messages.iter().any(|m| m.content == "old reply"));
    assert_eq!(mode_states.auto.pending_for("test-session"), None);
    assert!(
        !mode_states.plan.is_active(),
        "/new clears model-entered Plan state"
    );
}
