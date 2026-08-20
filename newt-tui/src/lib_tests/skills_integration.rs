use super::*;
use std::fs;

fn write_skill(root: &std::path::Path, name: &str, desc: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\nFull body of {name}.\n"),
    )
    .unwrap();
}

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_index_includes_discovered_skill_name_and_description() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_skill(tmp.path(), "commit-style", "How this repo writes commits");

    let block = skills_index_for_prompt(&[tmp.path().to_path_buf()]).expect("an index block");
    assert!(block.contains("Available skills (call `use_skill` to load one):"));
    assert!(block.contains("commit-style: How this repo writes commits"));
    // Progressive disclosure: the body must NOT appear in the index.
    assert!(!block.contains("Full body of commit-style."));
}

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_index_is_none_when_no_skills() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(skills_index_for_prompt(&[tmp.path().to_path_buf()]).is_none());
}

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_index_unions_search_path_first_dir_wins() {
    // A skill of the same name in two dirs: the first dir on the path wins.
    let a = tempfile::TempDir::new().unwrap();
    let b = tempfile::TempDir::new().unwrap();
    write_skill(a.path(), "commit-style", "newt copy");
    write_skill(b.path(), "commit-style", "claude copy");
    write_skill(b.path(), "judge", "scoring");

    let block = skills_index_for_prompt(&[a.path().to_path_buf(), b.path().to_path_buf()])
        .expect("an index block");
    // First dir's description wins; second dir's same-named skill is shadowed.
    assert!(block.contains("commit-style: newt copy"));
    assert!(!block.contains("claude copy"));
    // But unique skills from later dirs are still included.
    assert!(block.contains("judge: scoring"));
}

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_fallback_uses_canonical_default_soul() {
    // Regression: the no-soul fallback used to be a private copy of the
    // identity string that drifted from newt-core's DEFAULT_SOUL. It must
    // now embed the canonical constant verbatim so the two can't diverge.
    let tmp = tempfile::TempDir::new().unwrap();
    let prompt = build_system_prompt_with_soul(tmp.path().to_str().unwrap(), None, "test-plan.md");
    assert!(
        prompt.contains(newt_core::DEFAULT_SOUL),
        "fallback must embed newt_core::DEFAULT_SOUL verbatim"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_names_the_per_session_plan_path() {
    // Issue #220: the plan instruction must reference the per-session path
    // passed in, not the old fixed `.newt/plan.md`.
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().to_str().unwrap();
    let path_a = newt_core::session_plan_path("sess-aaaa");
    let path_a = path_a.to_string_lossy();
    let prompt_a = build_system_prompt_with_soul(ws, None, &path_a);
    assert!(
        prompt_a.contains(path_a.as_ref()),
        "prompt must name the session plan path"
    );
    assert!(
        prompt_a.contains("Plan before coding"),
        "the plan instruction must still be present (now injected, not in DEFAULT_SOUL)"
    );

    // Two different sessions get two different plan paths — the collision fix.
    let path_b = newt_core::session_plan_path("sess-bbbb");
    let prompt_b = build_system_prompt_with_soul(ws, None, &path_b.to_string_lossy());
    assert!(prompt_b.contains(&*path_b.to_string_lossy()));
    assert!(
        !prompt_b.contains(path_a.as_ref()),
        "sessions must not share a path"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn default_soul_no_longer_hardcodes_a_plan_path() {
    // The plan path moved out of the const so it can be per-session and so
    // custom souls also get the guidance (issue #220).
    assert!(!newt_core::DEFAULT_SOUL.contains("plan.md"));
    // A custom soul (no plan text of its own) still gets the injected block.
    let tmp = tempfile::TempDir::new().unwrap();
    let prompt = build_system_prompt_with_soul(
        tmp.path().to_str().unwrap(),
        Some("You are a custom agent."),
        ".scratch/sessions/xyz/plan.md",
    );
    assert!(prompt.contains("You are a custom agent."));
    assert!(prompt.contains(".scratch/sessions/xyz/plan.md"));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn registered_agents_provider_block_reaches_prompt() {
    // A registered AgentsProvider should compose its instruction block into
    // the assembled system prompt via build_system_prompt_additions.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Run just check before PRs.").unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::AgentsProvider::new(true, None));
    let ctx = newt_core::SessionContext {
        workspace: dir.path().to_string_lossy().into_owned(),
        session_id: "s".into(),
    };
    memory.initialize_all(&ctx).await;

    let prompt = rebuild_system_prompt(
        dir.path().to_str().unwrap(),
        &memory,
        None,
        "test-conversation",
    );
    assert!(prompt.contains("# Project instructions"));
    assert!(prompt.contains("Run just check before PRs."));
}

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
        .sync_all("old task", "old reply", &newt_core::TurnMetrics::default())
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
        .sync_all("old task", "old reply", &newt_core::TurnMetrics::default())
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

#[test]
fn conversation_commands_parse_expected_actions() {
    assert_eq!(
        parse_conversation_command("/conversation list").unwrap(),
        ConversationCommand::List
    );
    assert_eq!(
        parse_conversation_command("/conversation show abc").unwrap(),
        ConversationCommand::Show("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation restore abc").unwrap(),
        ConversationCommand::Restore("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation rename abc A better title").unwrap(),
        ConversationCommand::Rename {
            id: "abc".into(),
            title: "A better title".into()
        }
    );
    assert_eq!(
        parse_conversation_command("/conversation delete abc").unwrap(),
        ConversationCommand::Delete("abc".into())
    );
    assert_eq!(
        parse_conversation_command("/conversation rm abc").unwrap(),
        ConversationCommand::Delete("abc".into())
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn help_documents_conversation_rm_alias() {
    assert!(help_lines()
        .iter()
        .any(|line| line.contains("/conversation rm <id>")));
}

// -- /recall (Step 17.4, #246) ------------------------------------------

/// A real store on tempdirs, mirroring the conversation-command tests.
/// Returns the dirs so they outlive the store.
fn recall_test_store() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    newt_core::ConversationStore,
) {
    let state = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
    (state, workspace, store)
}

#[test]
fn recall_commands_parse_expected_actions() {
    assert_eq!(
        parse_recall_command("/recall").unwrap(),
        RecallCommand::Browse
    );
    assert_eq!(
        parse_recall_command("/recall   ").unwrap(),
        RecallCommand::Browse
    );
    assert_eq!(
        parse_recall_command("/recall tokio panic").unwrap(),
        RecallCommand::Search("tokio panic".into())
    );
    // `/recallx` is some other (unknown) command, not `/recall x`.
    assert!(parse_recall_command("/recallx").is_err());
    assert!(parse_recall_command("/conversation list").is_err());
}

#[test]
fn resume_commands_parse_expected_actions() {
    assert_eq!(parse_resume_command("/resume"), ResumeCommand::Browse);
    assert_eq!(parse_resume_command("/resume   "), ResumeCommand::Browse);
    assert_eq!(parse_resume_command("/resume 3"), ResumeCommand::Select(3));
    assert_eq!(
        parse_resume_command("/resume tokio panic"),
        ResumeCommand::Query("tokio panic".into())
    );
    // #1030: a big all-digits token (the displayed short id is ~19 nanos
    // digits) is an id PREFIX, not a row number — routed to Query/resolve_id
    // so the short id the UI prints is typeable.
    assert_eq!(
        parse_resume_command("/resume 175200000000"),
        ResumeCommand::Query("175200000000".into())
    );
}

#[test]
fn help_lists_the_resume_command() {
    assert!(help_lines().iter().any(|l| l.contains("/resume")));
}

#[test]
fn resume_browse_numbers_rows_and_marks_liveness() {
    let (_state, _ws, mut store) = recall_test_store();
    store.set_owner_for_test("host", "boot", 1);
    let a = store.create("Alpha", None).unwrap();
    let b = store.create("Bravo", None).unwrap();
    // Bravo is held by a live owner -> ● ; Alpha is unclaimed but is the
    // "active" id passed below -> ▶.
    store.set_liveness_for_test(|_, _| true);
    store.claim(&b).unwrap();
    let (msg, ids) = resume_browse_message(&store, &a).unwrap();
    // list() is MRU, so Bravo (created last) is row 1, Alpha row 2.
    assert_eq!(ids, vec![b.clone(), a.clone()]);
    assert!(msg.contains("1. ●"), "held conversation marked live: {msg}");
    assert!(msg.contains("2. ▶"), "the active id marked current: {msg}");
    assert!(msg.contains("Alpha") && msg.contains("Bravo"));
}

#[test]
fn resume_search_lists_one_row_per_conversation() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Parser work", None).unwrap();
    store
        .append_turn(&id, "fix the parser tokens", "done")
        .unwrap();
    store.append_turn(&id, "more parser tokens", "ok").unwrap();
    let (msg, ids) = resume_search_message(&store, "parser", "other-active").unwrap();
    // Two matching turns in ONE conversation -> a single numbered row.
    assert_eq!(ids, vec![id]);
    assert!(msg.contains("1. "), "numbered: {msg}");
    assert!(msg.contains("Parser work"));
}

#[test]
fn roadmap_commands_parse_expected_actions() {
    use newt_core::plan::NodeKind;
    assert_eq!(
        parse_roadmap_command("/roadmap").unwrap(),
        RoadmapCommand::Show(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap list").unwrap(),
        RoadmapCommand::List
    );
    assert_eq!(
        parse_roadmap_command("/roadmap show rm-1").unwrap(),
        RoadmapCommand::Show(Some("rm-1".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap new Mermaid in Rust").unwrap(),
        RoadmapCommand::New("Mermaid in Rust".into())
    );
    assert_eq!(
        parse_roadmap_command("/roadmap use rm-1").unwrap(),
        RoadmapCommand::Use("rm-1".into())
    );
    assert_eq!(
        parse_roadmap_command("/roadmap add phase Build the parser").unwrap(),
        RoadmapCommand::Add {
            kind: NodeKind::Phase,
            title: "Build the parser".into(),
            under: None
        }
    );
    assert_eq!(
        parse_roadmap_command("/roadmap add plan Implement it under phase-1").unwrap(),
        RoadmapCommand::Add {
            kind: NodeKind::Plan,
            title: "Implement it".into(),
            under: Some("phase-1".into())
        }
    );
    assert!(parse_roadmap_command("/roadmap new").is_err());
    assert!(parse_roadmap_command("/roadmap add nonsense x").is_err());
}

#[test]
fn help_lists_the_roadmap_and_tree_commands() {
    assert!(help_lines().iter().any(|l| l.contains("/roadmap")));
    assert!(help_lines().iter().any(|l| l.contains("/tree")));
}

#[test]
fn render_roadmap_tree_outlines_nodes_by_depth() {
    let toml = "\
[[subtask]]
id = \"road\"
instruction = \"the roadmap\"
kind = \"roadmap\"

[[subtask]]
id = \"phase-1\"
instruction = \"phase one\"
kind = \"phase\"
parent = \"road\"
";
    let tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    let rm = newt_core::Roadmap {
        id: "rm-123456789012".into(),
        title: "Demo".into(),
        tree,
    };
    let out = render_roadmap_tree(&rm);
    assert!(out.contains("Roadmap: Demo"));
    assert!(out.contains("roadmap [road]"));
    assert!(out.contains("phase [phase-1]"));
    // The child (phase-1) is indented deeper than its parent (road).
    let road_indent = out
        .lines()
        .find(|l| l.contains("[road]"))
        .unwrap()
        .find('○');
    let phase_indent = out
        .lines()
        .find(|l| l.contains("[phase-1]"))
        .unwrap()
        .find('○');
    assert!(phase_indent > road_indent, "child indented deeper: {out}");
}

#[test]
fn empty_roadmap_renders_a_hint() {
    let rm = newt_core::Roadmap {
        id: "rm-1".into(),
        title: "Empty".into(),
        tree: newt_core::plan::Plan::default(),
    };
    assert!(render_roadmap_tree(&rm).contains("no nodes yet"));
}

#[test]
fn next_roadmap_node_id_avoids_collisions() {
    let mut tree = newt_core::plan::Plan::default();
    assert_eq!(next_roadmap_node_id(&tree), "node-1");
    tree.subtasks.push(newt_core::plan::Subtask::node(
        "node-1",
        "x",
        newt_core::plan::NodeKind::Task,
        None,
    ));
    assert_eq!(next_roadmap_node_id(&tree), "node-2");
}

#[test]
fn roadmap_drive_subcommands_parse() {
    assert_eq!(
        parse_roadmap_command("/roadmap next").unwrap(),
        RoadmapCommand::Next
    );
    assert_eq!(
        parse_roadmap_command("/roadmap work").unwrap(),
        RoadmapCommand::Next
    );
    assert_eq!(
        parse_roadmap_command("/roadmap bind").unwrap(),
        RoadmapCommand::Bind(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap bind node-3").unwrap(),
        RoadmapCommand::Bind(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap done").unwrap(),
        RoadmapCommand::Done(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap done node-3").unwrap(),
        RoadmapCommand::Done(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap eval").unwrap(),
        RoadmapCommand::Eval(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap eval node-3").unwrap(),
        RoadmapCommand::Eval(Some("node-3".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap drive").unwrap(),
        RoadmapCommand::Drive
    );
    // #1062: task <node> commit [sha] — HEAD when the sha is omitted.
    assert_eq!(
        parse_roadmap_command("/roadmap task node-4 commit").unwrap(),
        RoadmapCommand::TaskCommit {
            node: "node-4".into(),
            sha: None
        }
    );
    assert_eq!(
        parse_roadmap_command("/roadmap task node-4 commit b56fefa").unwrap(),
        RoadmapCommand::TaskCommit {
            node: "node-4".into(),
            sha: Some("b56fefa".into())
        }
    );
    assert!(
        parse_roadmap_command("/roadmap task node-4").is_err(),
        "task without `commit` is a usage error"
    );
    assert!(
        parse_roadmap_command("/roadmap task").is_err(),
        "task without a node is a usage error"
    );
}

/// #1062 auto-capture decision (pure): a new commit in a bound Plan's turn
/// targets that Plan's next uncaptured Task; no commit / unbound / no ready
/// task → nothing captured.
#[test]
fn autocapture_target_picks_the_bound_plans_next_task_on_a_new_commit() {
    let toml = r#"
[[subtask]]
id = "pl"
instruction = "plan"
kind = "plan"
conversation_id = "conv-1"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "pl"
"#;
    let mut tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    // No new commit → nothing.
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "abc"),
        None
    );
    // New commit + bound Plan with a pending Task → that Task.
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "def"),
        Some("t1".into())
    );
    // A first commit from an unborn HEAD (None before) still counts.
    assert_eq!(
        autocapture_target(&tree, "conv-1", None, "def"),
        Some("t1".into())
    );
    // A conversation NOT bound to any Plan → nothing.
    assert_eq!(autocapture_target(&tree, "other-conv", None, "def"), None);
    // Once the Plan's only Task is captured, a later commit finds no target.
    tree.set_artifact_commit("t1", "def", None);
    assert_eq!(
        autocapture_target(&tree, "conv-1", Some("abc"), "ghi"),
        None
    );
}

#[test]
fn render_marks_the_next_ready_node_with_the_cursor() {
    // road (branch, pending) → task-1 (leaf, pending). next_ready_node = task-1.
    let toml = "\
[[subtask]]
id = \"road\"
instruction = \"the roadmap\"
kind = \"roadmap\"

[[subtask]]
id = \"task-1\"
instruction = \"do it\"
kind = \"task\"
parent = \"road\"
";
    let tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    let rm = newt_core::Roadmap {
        id: "rm-1".into(),
        title: "Demo".into(),
        tree,
    };
    let out = render_roadmap_tree(&rm);
    // The cursor ▶ sits on task-1 (the next-ready node), not on the branch.
    let task_line = out.lines().find(|l| l.contains("[task-1]")).unwrap();
    assert!(task_line.contains('▶'), "cursor on next-ready node: {out}");
    let road_line = out.lines().find(|l| l.contains("[road]")).unwrap();
    assert!(!road_line.contains('▶'), "branch is not the cursor: {out}");
}

#[test]
fn roadmap_bind_eval_and_done_drive_a_node_through_its_status() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd"; // a stand-in active conversation id
    let mut active_roadmap: Option<String> = None;

    // Author a roadmap with one Plan node.
    handle_roadmap_command(
        "/roadmap new Build it",
        &store,
        &mut active_roadmap,
        conv,
        ws,
    )
    .unwrap();
    handle_roadmap_command(
        "/roadmap add plan Parser",
        &store,
        &mut active_roadmap,
        conv,
        ws,
    )
    .unwrap();
    let rm_id = active_roadmap.clone().unwrap();

    // /roadmap next reports the plan node needs a conversation (unbound).
    let next =
        handle_roadmap_command("/roadmap next", &store, &mut active_roadmap, conv, ws).unwrap();
    assert!(
        next.message.contains("Bind"),
        "unbound plan: {}",
        next.message
    );
    assert!(next.switch_to.is_none());

    // Bind THIS conversation to it → node goes Running and gets the conv id.
    handle_roadmap_command("/roadmap bind", &store, &mut active_roadmap, conv, ws).unwrap();
    let node = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].clone();
    assert_eq!(node.status, newt_core::plan::SubtaskStatus::Running);
    assert_eq!(node.conversation_id.as_deref(), Some(conv));

    // /roadmap next now resumes-to-cursor: it hands back the bound conversation.
    let next2 =
        handle_roadmap_command("/roadmap next", &store, &mut active_roadmap, conv, ws).unwrap();
    assert_eq!(next2.switch_to.as_deref(), Some(conv));

    // /roadmap eval on the (childless) Plan node evaluates NOT done — no
    // objective evidence (no child tasks) — so it is not marked Done.
    let eval =
        handle_roadmap_command("/roadmap eval", &store, &mut active_roadmap, conv, ws).unwrap();
    assert!(
        eval.message.contains("not done yet"),
        "eval: {}",
        eval.message
    );
    assert_ne!(
        store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].status,
        newt_core::plan::SubtaskStatus::Done
    );

    // /roadmap done (defaulting to the bound node) marks it Done manually.
    handle_roadmap_command("/roadmap done", &store, &mut active_roadmap, conv, ws).unwrap();
    let done = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].status;
    assert_eq!(done, newt_core::plan::SubtaskStatus::Done);
}

// ── #1082 roadmap-as-code: /roadmap export + import ─────────────────────
// The file edge is injected (in-memory closures), so these stay in the
// fully-mocked unit tier — no fs I/O beyond the store the shared
// `recall_test_store()` helper already provides.

#[test]
fn roadmap_export_import_commands_parse() {
    assert_eq!(
        parse_roadmap_command("/roadmap export").unwrap(),
        RoadmapCommand::Export(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap export plans/r.toml").unwrap(),
        RoadmapCommand::Export(Some("plans/r.toml".into()))
    );
    assert_eq!(
        parse_roadmap_command("/roadmap import").unwrap(),
        RoadmapCommand::Import(None)
    );
    assert_eq!(
        parse_roadmap_command("/roadmap import /tmp/r.toml").unwrap(),
        RoadmapCommand::Import(Some("/tmp/r.toml".into()))
    );
}

// ── #1083: /roadmap issue — bind a node to the forge issue it realizes ──

#[test]
fn roadmap_issue_command_parses_plain_and_hash_numbers() {
    assert_eq!(
        parse_roadmap_command("/roadmap issue node-1 39").unwrap(),
        RoadmapCommand::IssueSet {
            node: "node-1".into(),
            number: 39
        }
    );
    // The friendly `#39` form binds the same.
    assert_eq!(
        parse_roadmap_command("/roadmap issue node-1 #39").unwrap(),
        RoadmapCommand::IssueSet {
            node: "node-1".into(),
            number: 39
        }
    );
    assert!(parse_roadmap_command("/roadmap issue node-1").is_err());
    assert!(parse_roadmap_command("/roadmap issue node-1 nope").is_err());
    assert!(parse_roadmap_command("/roadmap issue").is_err());
}

#[test]
fn roadmap_issue_binds_the_ref_on_any_node_and_rejects_unknown_ids() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;
    handle_roadmap_command("/roadmap new Gated", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command("/roadmap add phase P1", &store, &mut active, conv, ws).unwrap();
    let rm_id = active.clone().unwrap();

    // Binds on a PHASE (the gate is kind-agnostic), persists to the store.
    let out =
        handle_roadmap_command("/roadmap issue node-1 #39", &store, &mut active, conv, ws).unwrap();
    assert!(out.message.contains("issue #39"), "{}", out.message);
    let node = store.load_roadmap(&rm_id).unwrap().unwrap().tree.subtasks[0].clone();
    assert_eq!(node.artifact_ref.as_ref().and_then(|a| a.issue), Some(39));

    // Unknown node id fails loud, store unchanged.
    let err = handle_roadmap_command("/roadmap issue ghost 1", &store, &mut active, conv, ws)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no node `ghost`"), "{err}");
}

#[test]
fn roadmap_file_path_resolves_default_relative_and_absolute() {
    let default = roadmap_file_path("/ws", None);
    assert_eq!(
        default,
        std::path::Path::new("/ws").join(newt_core::roadmap_file::DEFAULT_ROADMAP_FILE)
    );
    // Relative args are workspace-relative — the file belongs to the repo.
    assert_eq!(
        roadmap_file_path("/ws", Some("plans/r.toml")),
        std::path::PathBuf::from("/ws/plans/r.toml")
    );
    assert_eq!(
        roadmap_file_path("/ws", Some("/abs/r.toml")),
        std::path::PathBuf::from("/abs/r.toml")
    );
}

#[test]
fn roadmap_export_then_import_round_trips_and_upserts_by_id() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;

    // Author a two-node roadmap, then export it through a fake fs.
    handle_roadmap_command("/roadmap new Chartered", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command("/roadmap add phase P1", &store, &mut active, conv, ws).unwrap();
    handle_roadmap_command(
        "/roadmap add plan Body under node-1",
        &store,
        &mut active,
        conv,
        ws,
    )
    .unwrap();
    let rm_id = active.clone().unwrap();

    let written: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let out = export_roadmap_to(
        &store,
        &rm_id,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_, text| {
            *written.borrow_mut() = Some(text.to_string());
            Ok(())
        },
    )
    .unwrap();
    assert!(out.message.contains("2 nodes"), "{}", out.message);
    let exported = written.borrow().clone().unwrap();

    // Drift the working copy past the export…
    handle_roadmap_command("/roadmap add phase Stray", &store, &mut active, conv, ws).unwrap();
    assert_eq!(
        store
            .load_roadmap(&rm_id)
            .unwrap()
            .unwrap()
            .tree
            .subtasks
            .len(),
        3
    );

    // …then import restores the repo authority IN PLACE (same id, updated).
    let mut fresh_active: Option<String> = None;
    let out = import_roadmap_from(
        &store,
        &mut fresh_active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok(exported.clone()),
    )
    .unwrap();
    assert!(out.message.contains("updated existing"), "{}", out.message);
    assert_eq!(fresh_active.as_deref(), Some(rm_id.as_str()));
    let restored = store.load_roadmap(&rm_id).unwrap().unwrap();
    assert_eq!(restored.tree.subtasks.len(), 2);
    assert_eq!(restored.title, "Chartered");

    // Round-trip is byte-identical: re-export matches the imported text.
    let rewritten: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    export_roadmap_to(&store, &rm_id, std::path::Path::new("/x"), &|_, text| {
        *rewritten.borrow_mut() = Some(text.to_string());
        Ok(())
    })
    .unwrap();
    assert_eq!(rewritten.borrow().clone().unwrap(), exported);
}

#[test]
fn roadmap_import_into_empty_workspace_creates_and_activates() {
    let (_state, _ws, store) = recall_test_store();
    let text = newt_core::roadmap_file::RoadmapFile::new(
        "rm-fresh",
        "Bootstrapped",
        newt_core::plan::Plan::default(),
    )
    .to_toml_string()
    .unwrap();
    let mut active: Option<String> = None;
    let out = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok(text.clone()),
    )
    .unwrap();
    assert!(out.message.contains("created new"), "{}", out.message);
    assert_eq!(active.as_deref(), Some("rm-fresh"));
    assert!(store.load_roadmap("rm-fresh").unwrap().is_some());
}

#[test]
fn roadmap_import_corrupt_file_fails_loud_and_leaves_store_untouched() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let conv = "1781000000000000000-abcd";
    let mut active: Option<String> = None;
    handle_roadmap_command("/roadmap new Keep me", &store, &mut active, conv, ws).unwrap();
    let rm_id = active.clone().unwrap();

    // Corrupt file: parse fails BEFORE any store write; active id keeps.
    let err = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Ok("not = [toml".to_string()),
    )
    .unwrap_err();
    assert!(!err.to_string().is_empty());
    assert_eq!(active.as_deref(), Some(rm_id.as_str()));
    assert_eq!(store.list_roadmaps().unwrap().len(), 1);

    // Missing file: a friendly error naming the path and the bootstrap hint.
    let err = import_roadmap_from(
        &store,
        &mut active,
        std::path::Path::new("/repo/.newt/roadmap.toml"),
        &|_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone")),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains(".newt/roadmap.toml"), "{err}");
    assert!(err.contains("/roadmap export"), "{err}");
}

#[test]
fn roadmap_export_without_active_roadmap_is_a_friendly_error() {
    let (_state, ws_dir, store) = recall_test_store();
    let ws = ws_dir.path().to_str().unwrap();
    let mut active: Option<String> = None;
    let err = handle_roadmap_command(
        "/roadmap export",
        &store,
        &mut active,
        "1781000000000000000-abcd",
        ws,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no active roadmap"), "{err}");
}

#[test]
fn recall_garbage_only_query_renders_friendly_hint() {
    let (_state, _ws, store) = recall_test_store();
    // "AND" sanitizes to nothing (bare operator) — must come back as a
    // friendly Ok message, never through the `error:` path.
    let msg = handle_recall_command("/recall AND", &store).unwrap();
    assert!(msg.contains("Nothing searchable"), "got: {msg}");
    assert!(msg.contains("Try plain keywords"), "got: {msg}");
}

#[test]
fn recall_browse_orders_by_activity_tick_with_short_ids() {
    let (_state, _ws, store) = recall_test_store();
    let alpha = store.create("Alpha task", None).unwrap();
    store
        .append_turn(&alpha, "alpha question", "alpha answer")
        .unwrap();
    let beta = store.create("Beta task", None).unwrap();
    store
        .append_turn(&beta, "beta question", "beta answer")
        .unwrap();
    // Reactivate alpha: a new turn gives it the highest activity tick.
    store
        .append_turn(&alpha, "alpha follow-up", "alpha again")
        .unwrap();

    let msg = handle_recall_command("/recall", &store).unwrap();
    assert!(msg.starts_with("Recent conversations (most recent first):"));
    let alpha_pos = msg.find("Alpha task").unwrap();
    let beta_pos = msg.find("Beta task").unwrap();
    assert!(alpha_pos < beta_pos, "most recently active first:\n{msg}");
    // Ids render as 12-char prefixes, never in full.
    assert!(msg.contains(short_conversation_id(&alpha)));
    assert!(!msg.contains(&alpha));
    assert!(!msg.contains(&beta));
    // Turn counts + the last-activity display claim (§6: a claim, hence ~).
    assert!(msg.contains("(2 turns, last active ~"), "got: {msg}");
    assert!(msg.contains("(1 turns, last active ~"), "got: {msg}");
    assert!(msg.ends_with("Restore with /conversation restore <id>."));
}

#[test]
fn recall_browse_empty_store_message() {
    let (_state, _ws, store) = recall_test_store();
    assert_eq!(
        recall_browse_message(&store).unwrap(),
        "No saved conversations for this workspace."
    );
}

#[test]
fn recall_browse_truncates_to_limit_with_overflow_line() {
    let (_state, _ws, store) = recall_test_store();
    for i in 0..(RECALL_LIMIT + 2) {
        store.create(&format!("conv-{i:02}"), None).unwrap();
    }
    let msg = recall_browse_message(&store).unwrap();
    // The two least-recently-created fall off the end of the browse view.
    assert!(!msg.contains("conv-00"), "got: {msg}");
    assert!(!msg.contains("conv-01"), "got: {msg}");
    assert!(msg.contains("conv-02"));
    assert!(msg.contains(&format!("conv-{:02}", RECALL_LIMIT + 1)));
    assert!(msg.contains("… 2 more — /conversation list shows all."));
}

#[test]
fn recall_search_renders_snippets_and_footer() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Login bug", None).unwrap();
    store
        .append_turn(
            &id,
            "the login form crashes on submit",
            "fixed the crash in the submit handler",
        )
        .unwrap();
    let other = store.create("Docs chore", None).unwrap();
    store
        .append_turn(&other, "write the readme", "done")
        .unwrap();

    let msg = handle_recall_command("/recall login", &store).unwrap();
    assert!(msg.starts_with("Recall matches for `login`:"), "got: {msg}");
    assert!(msg.contains(short_conversation_id(&id)));
    assert!(!msg.contains(&id), "full ids must not render:\n{msg}");
    assert!(msg.contains("Login bug"));
    assert!(msg.contains("  ·  seq "), "got: {msg}");
    // The FTS5 `>>>`/`<<<` match markers render as `«`/`»` highlights.
    assert!(msg.contains("«login»"), "got: {msg}");
    assert!(msg.contains("form crashes on submit"), "got: {msg}");
    assert!(!msg.contains("Docs chore"), "non-hit leaked:\n{msg}");
    assert!(msg.ends_with("Restore with /conversation restore <id>."));
}

#[test]
fn recall_search_no_matches_message() {
    let (_state, _ws, store) = recall_test_store();
    let id = store.create("Something", None).unwrap();
    store
        .append_turn(&id, "unrelated work", "still unrelated")
        .unwrap();
    assert_eq!(
        recall_search_message(&store, "zebra").unwrap(),
        "No matches for `zebra` in this workspace's conversations."
    );
}

#[test]
fn help_documents_recall_command() {
    assert!(help_lines()
        .iter()
        .any(|line| line.contains("/recall [query]")));
}

#[test]
fn wal_fallback_startup_notice_surfaces_only_when_present() {
    // N7 (#261 review): the seam the run loop feeds the store's notice
    // through. Present → a visible warning naming the fallback + cause.
    let msg = wal_fallback_startup_notice(Some("locking protocol")).unwrap();
    assert!(msg.contains("journal_mode=DELETE"), "got: {msg}");
    assert!(msg.contains("locking protocol"), "got: {msg}");
    // Absent → silence.
    assert_eq!(wal_fallback_startup_notice(None), None);
    // A healthy local store reports no fallback end-to-end.
    let (_state, _ws, store) = recall_test_store();
    assert_eq!(
        wal_fallback_startup_notice(store.wal_fallback_notice()),
        None
    );
}

#[test]
fn recall_title_falls_back_to_first_user_turn_at_render() {
    let (_state, _ws, store) = recall_test_store();
    // An empty stored title (can't happen via the TUI create path —
    // `conversation_title_from_task` never returns empty — but a record
    // written elsewhere can carry one).
    let id = store.create("", None).unwrap();
    let task = "alpha ".repeat(20);
    store.append_turn(&id, &task, "reply").unwrap();
    let title = recall_display_title(&store, &id, "");
    assert_eq!(title.chars().count(), 60);
    assert!(task.starts_with(&title));
    // Empty title and no turns at all → "(untitled)".
    let bare = store.create("  ", None).unwrap();
    assert_eq!(recall_display_title(&store, &bare, "  "), "(untitled)");
    // And the browse view actually uses the fallback.
    let msg = recall_browse_message(&store).unwrap();
    assert!(msg.contains("(untitled)"), "got: {msg}");
    assert!(msg.contains(title.trim_end()), "got: {msg}");
    // A present title is used verbatim — no record load needed.
    assert_eq!(
        recall_display_title(&store, "no-such-id", " Kept title "),
        "Kept title"
    );
}

#[test]
fn recall_claim_timestamp_formats_and_clamps() {
    assert_eq!(claim_timestamp(0), "1970-01-01 00:00 UTC");
    // 2026-06-11 00:00:00 UTC in nanos.
    assert_eq!(
        claim_timestamp(1_781_136_000 * 1_000_000_000),
        "2026-06-11 00:00 UTC"
    );
    assert_eq!(claim_timestamp(u128::MAX), "unknown");
}

#[test]
fn recall_readable_snippet_flattens_and_marks() {
    assert_eq!(
        readable_snippet("…the >>>tokio<<< runtime\n  panicked…"),
        "…the «tokio» runtime panicked…"
    );
}

#[test]
fn recall_short_id_is_a_restorable_prefix() {
    let id = newt_core::new_conversation_id();
    let short = short_conversation_id(&id);
    assert_eq!(short.len(), 12);
    assert!(id.starts_with(short));
    // Shorter-than-prefix ids pass through whole.
    assert_eq!(short_conversation_id("abc"), "abc");
}

// -- /compress (Step 18.6, #247) ------------------------------------------

#[test]
fn compress_commands_parse_expected_focus() {
    assert_eq!(parse_compress_command("/compress").unwrap(), None);
    assert_eq!(parse_compress_command("/compress   ").unwrap(), None);
    assert_eq!(
        parse_compress_command("/compress auth token handling").unwrap(),
        Some("auth token handling".into())
    );
    // The focus is opaque free text: FTS5-hostile operators and a
    // secret-looking string parse fine — redaction is the pipeline's
    // job, not the parser's.
    assert_eq!(
        parse_compress_command("/compress AND \"NEAR/2\" sk-aaaaaaaaaaaaaaaaaaaaaaaa1234").unwrap(),
        Some("AND \"NEAR/2\" sk-aaaaaaaaaaaaaaaaaaaaaaaa1234".into())
    );
    // `/compressx` is some other (unknown) command, not `/compress x`.
    assert!(parse_compress_command("/compressx").is_err());
    assert!(parse_compress_command("/memory").is_err());
}

/// A session memory with `turns` fat user/assistant turns — enough
/// summarizable middle for the pipeline to fire without token pressure.
async fn compressible_memory(turns: usize) -> newt_core::MemoryManager {
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(50));
    memory
        .sync_all(
            "ORIGINAL TASK: port the parser",
            "starting on it",
            &newt_core::TurnMetrics::default(),
        )
        .await;
    for i in 0..turns {
        memory
            .sync_all(
                &format!("question {i} {}", "u".repeat(300)),
                &format!("answer {i} {}", "v".repeat(300)),
                &newt_core::TurnMetrics::default(),
            )
            .await;
    }
    memory
}

/// The command's real parts end to end: wire view → shared pipeline →
/// honesty feedback whose numbers match the actual outcome → write-back,
/// so the NEXT turn really sends the compressed working set.
#[tokio::test]
async fn manual_compress_shrinks_session_and_notice_is_truthful() {
    let mut memory = compressible_memory(12).await;
    let system = "you are newt";
    let wire = session_wire_view(&memory, system);
    assert!(
        wire.last().is_some_and(|m| m["role"] == "assistant"),
        "the empty task slot must be popped from the wire view"
    );
    let before_len = wire.len();

    let summarizer: newt_core::Summarizer =
        Box::new(|_req: String| -> newt_core::SummarizeFuture {
            Box::pin(async { Ok("## Active Task\nMANUAL SUMMARY".to_string()) })
        });
    let mut state = newt_core::CompressState::new();
    let outcome = newt_core::compress_user_initiated(
        &wire,
        None,
        Some(&*summarizer),
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;

    assert!(outcome.fired);
    assert_eq!(outcome.messages_before, before_len);
    assert!(outcome.messages_after < outcome.messages_before);
    assert!(outcome.tokens_after < outcome.tokens_before);

    // The notice numbers are the outcome's numbers — no independent
    // arithmetic that could drift from what actually happened.
    let msg = compress_feedback_message(&outcome);
    assert!(
        msg.contains(&format!(
            "context compressed: {} → {} messages, ~{} → ~{} est. tokens",
            outcome.messages_before,
            outcome.messages_after,
            outcome.tokens_before,
            outcome.tokens_after
        )),
        "got: {msg}"
    );
    assert!(msg.contains("prune + summary"), "got: {msg}");
    assert!(!msg.contains("note: no token savings"), "got: {msg}");

    // Write-back through the existing replace seam: the next build is
    // the compressed set (marker included), not the raw history.
    memory.restore_turns(&wire_messages_to_turns(&outcome.messages));
    let next = memory.build_messages(system, "next task");
    assert!(
        next.len() < before_len,
        "next turn must send the compressed set"
    );
    assert!(next.iter().any(
        |m| m.content.starts_with(newt_core::agentic::SUMMARY_PREFIX)
            && m.content.contains("MANUAL SUMMARY")
    ));
    // The fired manual run shows up in the /memory counters.
    assert_eq!(state.counters().compressions, 1);
}

/// No-op honesty: an incompressible session reports "no compression
/// possible" and never claims savings.
#[tokio::test]
async fn manual_compress_noop_reports_no_compression_possible() {
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(50));
    memory
        .sync_all("hi", "hello", &newt_core::TurnMetrics::default())
        .await;
    let wire = session_wire_view(&memory, "you are newt");
    let mut state = newt_core::CompressState::new();
    let outcome = newt_core::compress_user_initiated(
        &wire,
        None,
        None,
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;

    assert!(!outcome.fired);
    let msg = compress_feedback_message(&outcome);
    assert!(msg.contains("no compression possible"), "got: {msg}");
    assert!(
        !msg.contains("context compressed"),
        "must not claim savings that didn't happen: {msg}"
    );
    assert_eq!(state.counters().compressions, 0);
}

/// Fired-but-no-token-savings gets the explicit hermes honesty note
/// instead of an implied win.
#[test]
fn compress_feedback_flags_fired_without_token_savings() {
    let outcome = newt_core::ManualCompressOutcome {
        messages: Vec::new(),
        fired: true,
        messages_before: 10,
        messages_after: 6,
        tokens_before: 800,
        tokens_after: 850,
        how: "prune + summary",
        notice: None,
    };
    let msg = compress_feedback_message(&outcome);
    assert!(msg.contains("10 → 6 messages"), "got: {msg}");
    assert!(msg.contains("note: no token savings"), "got: {msg}");
}

/// A secret typed into the focus never reaches the summarizer request —
/// the focus rides the same redaction the rendered middle gets.
#[tokio::test]
async fn compress_focus_secret_never_reaches_summarizer() {
    let memory = compressible_memory(12).await;
    let wire = session_wire_view(&memory, "you are newt");
    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = prompts.clone();
    let summarizer: newt_core::Summarizer =
        Box::new(move |req: String| -> newt_core::SummarizeFuture {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(req);
                Ok("SUMMARY".to_string())
            })
        });
    let mut state = newt_core::CompressState::new();
    let secret = "sk-aaaaaaaaaaaaaaaaaaaaaaaa1234";
    let focus = format!("the login flow around {secret}");
    let outcome = newt_core::compress_user_initiated(
        &wire,
        Some(&focus),
        Some(&*summarizer),
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;
    assert!(outcome.fired, "the summarizer path must have run");

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("emphasize anything about"),
        "{}",
        prompts[0]
    );
    assert!(prompts[0].contains("the login flow"), "{}", prompts[0]);
    assert!(
        !prompts[0].contains(secret),
        "focus secret leaked into the summarizer request"
    );
    assert!(prompts[0].contains("[REDACTED]"));
}

#[test]
fn memory_compress_section_renders_states() {
    // Fresh session: nothing recorded, enabled, no reclaim figure.
    let fresh = memory_compress_section(&newt_core::CompressCounters {
        compressions: 0,
        strikes: 0,
        disabled: false,
        last_reclaim: None,
    });
    assert!(fresh.contains("compressions this session: 0"), "{fresh}");
    assert!(!fresh.contains("last reclaimed"), "{fresh}");
    assert!(fresh.contains("strikes: 0/2"), "{fresh}");
    assert!(fresh.contains("auto-compression: enabled"), "{fresh}");
    assert!(
        !fresh.contains("/new resets it"),
        "the reset hint shows only when latched: {fresh}"
    );

    // Post-compression: count + last reclaim percentage surface.
    let post = memory_compress_section(&newt_core::CompressCounters {
        compressions: 2,
        strikes: 1,
        disabled: false,
        last_reclaim: Some(0.07),
    });
    assert!(post.contains("compressions this session: 2"), "{post}");
    assert!(post.contains("(last reclaimed 7%)"), "{post}");
    assert!(post.contains("strikes: 1/2"), "{post}");
    assert!(post.contains("auto-compression: enabled"), "{post}");

    // Latched: disabled status with the truthful "/new resets it" hint
    // (true since #267's F4 — `handle_new_conversation` resets the state).
    let latched = memory_compress_section(&newt_core::CompressCounters {
        compressions: 3,
        strikes: 2,
        disabled: true,
        last_reclaim: Some(0.04),
    });
    assert!(latched.contains("strikes: 2/2"), "{latched}");
    assert!(latched.contains("auto-compression: disabled"), "{latched}");
    assert!(latched.contains("/new resets it"), "{latched}");

    // A negative reclaim (the pass GREW the estimate) is never clamped
    // into a "0% reclaimed" savings claim.
    let grew = memory_compress_section(&newt_core::CompressCounters {
        compressions: 1,
        strikes: 1,
        disabled: false,
        last_reclaim: Some(-0.06),
    });
    assert!(grew.contains("grew the estimate 6%"), "{grew}");
    assert!(!grew.contains("last reclaimed"), "{grew}");
}

#[test]
fn wire_messages_to_turns_pairs_and_lone_sides() {
    let compaction = format!("{}\nsummary body", newt_core::agentic::SUMMARY_PREFIX);
    let wire = vec![
        serde_json::json!({"role": "system", "content": "you are newt"}),
        serde_json::json!({"role": "user", "content": "the task"}),
        serde_json::json!({"role": "user", "content": compaction}),
        serde_json::json!({"role": "user", "content": "q1"}),
        serde_json::json!({"role": "assistant", "content": "a1"}),
    ];
    let turns = wire_messages_to_turns(&wire);
    // System dropped; task and compaction stand alone; q1/a1 pair up —
    // and the compaction is never mistaken for q-awaiting-reply.
    assert_eq!(turns.len(), 3);
    assert_eq!((&*turns[0].user, &*turns[0].assistant), ("the task", ""));
    assert_eq!(
        (&*turns[1].user, &*turns[1].assistant),
        (compaction.as_str(), "")
    );
    assert_eq!((&*turns[2].user, &*turns[2].assistant), ("q1", "a1"));
    // Token columns stay absent: these are no longer measured turns.
    assert!(turns
        .iter()
        .all(|t| t.tokens_in.is_none() && t.tokens_out.is_none()));
}

#[test]
fn help_documents_compress_command() {
    assert!(help_lines()
        .iter()
        .any(|line| line.contains("/compress [focus]")));
}

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
        .sync_all("old task", "old reply", &newt_core::TurnMetrics::default())
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

/// #1671: the pure name-matching rules behind `--resume <name>` — a unique
/// exact (case-insensitive) title wins, then a unique substring; ambiguity
/// and misses are hard errors that NAME the candidates.
#[test]
fn resume_by_name_matches_titles_and_refuses_ambiguity() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];

    // Case-insensitive EXACT match wins even when a superstring also exists.
    assert_eq!(
        resolve_conversation_by_name(&list, "Mesh Docking").unwrap(),
        "aaaa1111-0000-0000-0000-000000000000"
    );
    // Unique substring match resolves.
    assert_eq!(
        resolve_conversation_by_name(&list, "taxes").unwrap(),
        "cccc3333-0000-0000-0000-000000000000"
    );
    // Ambiguous substring: hard error listing the candidates.
    let err = resolve_conversation_by_name(&list, "docking")
        .unwrap_err()
        .to_string();
    assert!(err.contains("matches 2 conversations"), "{err}");
    assert!(err.contains("mesh docking"), "{err}");
    // No match: hard error, pointing at /resume browse.
    let err = resolve_conversation_by_name(&list, "nonesuch")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no conversation titled"), "{err}");
}

/// #1736: the consolidated `resolve_resume_target` is the ONE precedence chain
/// shared by startup `--resume <name>` and in-chat `/resume <thing>`. This is
/// the pure core — no store — so every resolution rule is unit-testable, and
/// because both front doors call it, equivalence between them is structural.
#[test]
fn resolve_resume_target_chains_id_prefix_title_then_ambiguity() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];

    // 1. exact conversation id.
    assert_eq!(
        crate::resolve_resume_target(&list, "cccc3333-0000-0000-0000-000000000000"),
        crate::ResumeNameResolve::Resolved("cccc3333-0000-0000-0000-000000000000".into())
    );
    // 2. unique id prefix.
    assert_eq!(
        crate::resolve_resume_target(&list, "aaaa1111"),
        crate::ResumeNameResolve::Resolved("aaaa1111-0000-0000-0000-000000000000".into())
    );
    // 3. exact (case-insensitive) title wins over the superstring sibling.
    assert_eq!(
        crate::resolve_resume_target(&list, "mesh docking"),
        crate::ResumeNameResolve::Resolved("aaaa1111-0000-0000-0000-000000000000".into())
    );
    // 4. unique title substring.
    assert_eq!(
        crate::resolve_resume_target(&list, "taxes"),
        crate::ResumeNameResolve::Resolved("cccc3333-0000-0000-0000-000000000000".into())
    );
    // 5. ambiguous title match → candidates for numbered selection.
    let amb = crate::resolve_resume_target(&list, "docking");
    let cands = match amb {
        crate::ResumeNameResolve::Ambiguous(c) => c,
        other => panic!("expected Ambiguous, got {other:?}"),
    };
    assert_eq!(cands.len(), 2);
    assert!(cands
        .iter()
        .any(|(id, _)| id == "aaaa1111-0000-0000-0000-000000000000"));
    assert!(cands
        .iter()
        .any(|(id, _)| id == "bbbb2222-0000-0000-0000-000000000000"));
    // 6. nothing matched → NotFound (the in-chat caller falls back to FTS).
    assert_eq!(
        crate::resolve_resume_target(&list, "nonesuch"),
        crate::ResumeNameResolve::NotFound
    );
    // A non-unique id prefix is NOT a silent resume — it falls through to
    // title matching, and with no title match it is NotFound (never a guess).
    let shared_prefix = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "one"),
        s("aaaa2222-0000-0000-0000-000000000000", "two"),
    ];
    assert_eq!(
        crate::resolve_resume_target(&shared_prefix, "aaaa"),
        crate::ResumeNameResolve::NotFound
    );
}

/// #1736: the title step of `resolve_resume_target` DELEGATES to
/// `resolve_conversation_by_name`, so the two front doors can never drift.
/// Every title query that resolves through the consolidated resolver must
/// resolve identically through the title-only startup resolver.
#[test]
fn resolve_resume_target_agrees_with_resolve_conversation_by_name_on_titles() {
    let s = |id: &str, title: &str| newt_core::ConversationSummary {
        id: id.into(),
        title: title.into(),
        persona: None,
        updated_at_unix_nanos: 0,
        turn_count: 1,
    };
    let list = vec![
        s("aaaa1111-0000-0000-0000-000000000000", "mesh docking"),
        s(
            "bbbb2222-0000-0000-0000-000000000000",
            "Mesh Docking Ceremony",
        ),
        s("cccc3333-0000-0000-0000-000000000000", "taxes TY2025"),
    ];
    for q in [
        "mesh docking",
        "Mesh Docking",
        "taxes",
        "docking",
        "nonesuch",
    ] {
        let via_target = crate::resolve_resume_target(&list, q);
        let via_name = resolve_conversation_by_name(&list, q);
        match (&via_target, via_name) {
            (crate::ResumeNameResolve::Resolved(a), Ok(b)) => assert_eq!(a, &b, "title {q:?}"),
            (
                crate::ResumeNameResolve::Ambiguous(a),
                Err(crate::TitleResolveError::Ambiguous { candidates: b, .. }),
            ) => {
                let a_ids: Vec<_> = a.iter().map(|(id, _)| id.as_str()).collect();
                let b_ids: Vec<_> = b.iter().map(|(id, _)| id.as_str()).collect();
                assert_eq!(a_ids, b_ids, "ambiguous title {q:?}");
            }
            (
                crate::ResumeNameResolve::NotFound,
                Err(crate::TitleResolveError::NotFound { .. }),
            ) => {}
            other => panic!("disagreement on {q:?}: {other:?}"),
        }
    }
}

/// #1736: an ambiguous `/resume <thing>` renders a NUMBERED, liveness-annotated
/// candidate listing so a follow-up `/resume <n>` selects one — not a bare
/// error. Mirrors the browse/search listing tests.
#[serial_test::serial(real_fs)]
#[test]
fn resume_ambiguous_message_numbers_candidates_for_selection() {
    let (_state, _ws, store) = recall_test_store();
    let a = store.create("mesh docking", None).unwrap();
    let b = store.create("Mesh Docking Ceremony", None).unwrap();
    let cands = vec![
        (a.clone(), "mesh docking".to_string()),
        (b.clone(), "Mesh Docking Ceremony".to_string()),
    ];
    let (msg, ids) = resume_ambiguous_message(&store, "docking", &cands, "other-active").unwrap();
    assert_eq!(ids, vec![a.clone(), b.clone()]);
    assert!(msg.contains("\"docking\" matches 2 conversations"), "{msg}");
    assert!(msg.contains("1. "), "numbered: {msg}");
    assert!(msg.contains("2. "), "numbered: {msg}");
    assert!(msg.contains("mesh docking") && msg.contains("Mesh Docking Ceremony"));
}

/// #1736: `/name <title>` is the ergonomic alias for `/rename <title>` — same
/// path, same semantics. Both verbs must be discoverable in /help, alongside
/// the basic conversation grammar (`/start`, `/resume`).
#[test]
fn help_lists_name_alias_and_resume_grammar() {
    let lines = help_lines();
    assert!(lines.iter().any(|l| l.contains("/start")), "missing /start");
    assert!(
        lines.iter().any(|l| l.contains("/resume")),
        "missing /resume"
    );
    assert!(
        lines.iter().any(|l| l.contains("/rename")),
        "missing /rename"
    );
    assert!(
        lines.iter().any(|l| l.contains("/name")),
        "missing /name alias"
    );
}

/// #1736: the live-owner/concurrent-newt protection is the claim-guard
/// `/resume` consults before reopening. A conversation a live newt already
/// holds must report `HeldBy` (so `/resume` refuses) — never `Claimed`.
#[serial_test::serial(real_fs)]
#[test]
fn resume_refuses_a_conversation_a_live_newt_owns() {
    let (_state, _ws, mut store) = recall_test_store();
    let id = store.create("Held by another", None).unwrap();
    // Plant a FOREIGN live owner (host A), then switch this store's identity
    // to a second newt (host B) and re-claim — the guard the `/resume` path
    // consults. A live, different owner must be refused with `HeldBy`.
    store.set_owner_for_test("hostA", "bootA", 1);
    store.set_liveness_for_test(|_, _| true);
    assert_eq!(
        store.claim(&id).unwrap(),
        newt_core::ClaimOutcome::Claimed,
        "first claim by host A should acquire"
    );
    store.set_owner_for_test("hostB", "bootB", 2);
    match store.claim(&id) {
        Ok(newt_core::ClaimOutcome::HeldBy { host, pid }) => {
            assert_eq!(host, "hostA");
            assert_eq!(pid, 1);
        }
        other => panic!("expected HeldBy for a live-owned conversation, got {other:?}"),
    }
}

#[serial_test::serial(real_fs)]
#[test]
fn should_auto_resume_only_for_latest_and_never_after_new() {
    // Config off / ephemeral / exact-id sessions never auto-resume.
    assert!(should_auto_resume(&SessionStart::ResumeLatest, false));
    assert!(!should_auto_resume(&SessionStart::Fresh, false));
    assert!(!should_auto_resume(&SessionStart::Ephemeral, false));
    assert!(!should_auto_resume(
        &SessionStart::ResumeExact("id".into()),
        false
    ));
    // /new opts the session out — auto-resume never undoes it.
    assert!(!should_auto_resume(&SessionStart::ResumeLatest, true));
}

/// Everything a resume needs, on temp dirs — the borrow-heavy parts stay
/// in each test (ConversationCommandContext borrows them all mutably).
fn resume_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    newt_core::ConversationStore,
    PersonaStore,
) {
    let state = tempfile::TempDir::new().unwrap();
    let workspace = tempfile::TempDir::new().unwrap();
    let store = newt_core::ConversationStore::new(state.path(), workspace.path(), 100).unwrap();
    let persona_dir = state.path().join("personas");
    fs::create_dir_all(&persona_dir).unwrap();
    (state, workspace, store, PersonaStore::new(persona_dir))
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
        memory.sync_all(&task, &big, &metrics(10 + i)).await;
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
    memory.sync_all("final task", &big, &metrics(600)).await;
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
