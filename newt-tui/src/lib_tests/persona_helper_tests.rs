use super::*;
use std::fs;

#[test]
fn persona_description_takes_first_nonempty_line_truncated() {
    let p = test_persona(
        "x",
        "\n\n# Reviewer persona\n\nbody text",
        std::path::PathBuf::from("/x.md"),
    );
    assert_eq!(p.description(), "Reviewer persona");

    let long = "a".repeat(200);
    let p = test_persona("x", &long, std::path::PathBuf::from("/x.md"));
    assert_eq!(p.description().chars().count(), 96, "capped at 96 chars");
}

#[test]
fn normalize_persona_name_lowercases_and_validates() {
    assert_eq!(normalize_persona_name("  ReViewer ").unwrap(), "reviewer");
    assert_eq!(normalize_persona_name("a-b_c9").unwrap(), "a-b_c9");
    assert!(normalize_persona_name("").is_err());
    assert!(normalize_persona_name("bad name").is_err());
    assert!(normalize_persona_name("näme").is_err());
}

#[cfg(feature = "rich-tui")]
#[test]
fn persona_save_is_atomic_and_refuses_existing_without_overwrite() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
    // First write creates the file.
    let path = store
        .save("bob", "+++\nrole = \"bob\"\n+++\n\nbody\n", false)
        .unwrap();
    assert!(path.exists());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("role = \"bob\""));
    // Second write WITHOUT overwrite → Exists, original untouched.
    assert!(matches!(
        store.save("bob", "NEW", false),
        Err(PersonaSaveError::Exists)
    ));
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("role = \"bob\""));
    // WITH overwrite → replaces atomically, no stray temp files.
    store.save("bob", "REPLACED", true).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "REPLACED");
    let stray = std::fs::read_dir(tmp.path().join("personas"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains(".tmp."));
    assert!(!stray, "no temp files remain after saves");
}

#[cfg(feature = "rich-tui")]
#[test]
fn persona_save_returns_the_normalized_on_disk_name() {
    // review-3 follow-up: the caller reports the returned path stem, which is
    // the NORMALIZED (lowercased) on-disk name — so the "saved persona 'x'"
    // confirmation matches the file, not the raw typed name.
    let tmp = tempfile::TempDir::new().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
    let path = store.save("MixedCase", "body", false).unwrap();
    assert_eq!(path.file_stem().unwrap().to_string_lossy(), "mixedcase");
}

#[cfg(feature = "rich-tui")]
#[test]
fn persona_overwrite_failure_preserves_the_original() {
    // review-3 §1: a failed replacement write leaves the original persona intact
    // (temp+rename never truncates in place). Failure injected via save_with.
    let tmp = tempfile::TempDir::new().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
    let path = store.save("bob", "ORIGINAL", false).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ORIGINAL");
    let r = store.save_with("bob", "NEW", true, |_p, _c| {
        Err(std::io::Error::other("boom"))
    });
    assert!(
        matches!(r, Err(PersonaSaveError::Io(_))),
        "the write failure surfaced"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "ORIGINAL",
        "original persona intact after a failed overwrite"
    );
}

#[test]
fn parse_persona_command_rejects_non_persona_and_bare_set() {
    assert!(parse_persona_command("/help").is_err());
    let err = parse_persona_command("/persona set").unwrap_err();
    assert!(err.to_string().contains("usage: /persona set"));
    let err = parse_persona_command("/persona switch").unwrap_err();
    assert!(err.to_string().contains("usage: /persona switch"));
    // `off` is an alias for clear.
    assert_eq!(
        parse_persona_command("/persona off").unwrap(),
        PersonaCommand::Clear
    );
}

#[test]
fn persona_status_reports_none_and_active() {
    assert_eq!(persona_status(None), "No active persona.");
    let p = test_persona(
        "terse",
        "Keep it short.",
        std::path::PathBuf::from("/p/terse.md"),
    );
    let status = persona_status(Some(&p));
    assert!(status.contains("Active persona: terse"));
    assert!(status.contains("Keep it short."));
    assert!(status.contains("/p/terse.md"));
}

/// FR-4 (#1041): `/persona show` lists a persona's bound skills.
#[test]
fn persona_status_lists_bound_skills() {
    let mut p = test_persona(
        "assistant",
        "Coach on state.",
        std::path::PathBuf::from("/p/assistant.md"),
    );
    p.profile.skills = Some(vec!["gila-personal-assistant".to_string()]);
    let status = persona_status(Some(&p));
    assert!(
        status.contains("skills: gila-personal-assistant"),
        "got: {status}"
    );
}

/// FR-4 (#1041): `missing_bound_skills` resolves declared names against the
/// real search dirs (this file's other `PersonaStore` tests already exercise
/// real fs, `#[serial(real_fs)]`) and reports only the ones that don't exist.
#[serial_test::serial(real_fs)]
#[test]
fn missing_bound_skills_reports_only_unresolved_names() {
    let tmp = tempfile::TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    let one = skills_dir.join("gila-personal-assistant");
    fs::create_dir_all(&one).unwrap();
    fs::write(
        one.join("SKILL.md"),
        "---\nname: gila-personal-assistant\ndescription: coach on modulex reports\n---\nBody.\n",
    )
    .unwrap();

    let dirs = vec![skills_dir];
    assert_eq!(
        missing_bound_skills(&["gila-personal-assistant".to_string()], &dirs),
        Vec::<String>::new(),
        "the declared skill resolves"
    );
    assert_eq!(
        missing_bound_skills(&["not-installed".to_string()], &dirs),
        vec!["not-installed".to_string()],
        "an unresolved declared skill is reported"
    );
    assert_eq!(
        missing_bound_skills(&[], &dirs),
        Vec::<String>::new(),
        "no declared skills ⇒ nothing missing"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn store_load_unknown_persona_lists_available() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("reviewer.md"), "Review things.").unwrap();
    let store = PersonaStore::new(dir);
    let err = store.load("nope").unwrap_err().to_string();
    assert!(err.contains("unknown persona `nope`"), "got: {err}");
    assert!(err.contains("reviewer"), "lists what IS available: {err}");
}

/// #1021: `GILA_SKILL` is a real, parseable `SKILL.md` — required frontmatter
/// present, matching `newt_skills::Skill::parse`'s expectations.
#[test]
fn gila_skill_template_parses() {
    let skill = newt_skills::Skill::parse(GILA_SKILL, "").unwrap();
    assert_eq!(skill.name, "gila-personal-assistant");
    assert!(!skill.description.is_empty());
}

#[serial_test::serial(real_fs)]
#[test]
fn seed_gila_skill_writes_when_missing_and_is_idempotent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    seed_gila_skill(&root).unwrap();
    let path = root.join("gila-personal-assistant").join("SKILL.md");
    assert_eq!(fs::read_to_string(&path).unwrap(), GILA_SKILL);

    // A user's locally-edited copy is NOT clobbered on the next seed.
    fs::write(&path, "edited by the user").unwrap();
    seed_gila_skill(&root).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited by the user");
}

#[serial_test::serial(real_fs)]
#[test]
fn seed_gila_skill_is_discoverable_via_newt_skills() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("skills");
    seed_gila_skill(&root).unwrap();
    let found = newt_skills::discover(&root);
    assert!(
        found.iter().any(|s| s.name == "gila-personal-assistant"),
        "seeded skill resolves via newt_skills::discover"
    );
}

#[serial_test::serial(real_fs)]
#[test]
fn store_load_rejects_invalid_name_and_empty_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("empty.md"), "   \n  \n").unwrap();
    let store = PersonaStore::new(dir);
    let err = store.load("bad name!").unwrap_err().to_string();
    assert!(err.contains("letters, numbers"), "got: {err}");
    let err = store.load("empty").unwrap_err().to_string();
    assert!(err.contains("persona `empty` is empty"), "got: {err}");
}

#[serial_test::serial(real_fs)]
#[test]
fn store_list_skips_empty_and_non_markdown_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("real.md"), "# Real persona").unwrap();
    fs::write(dir.join("blank.md"), "   ").unwrap();
    fs::write(dir.join("notes.txt"), "not a persona").unwrap();
    let store = PersonaStore::new(dir);
    let listed = store.list().unwrap();
    // The empty .md and the non-md file are skipped; `real` (and the seeded
    // coder/coach defaults, FR-16) are listed. Assert on membership rather
    // than an exact count so the shipped defaults don't pin the number.
    let names: std::collections::HashSet<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains("real"), "real persona listed");
    assert!(!names.contains("blank"), "empty .md skipped");
    assert!(!names.contains("notes"), "non-markdown skipped");
    let real = listed
        .iter()
        .find(|p| p.name == "real")
        .expect("real is listed");
    assert_eq!(real.description, "Real persona");
}

#[serial_test::serial(real_fs)]
#[test]
fn store_list_message_shows_none_when_all_personas_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    fs::create_dir_all(&dir).unwrap();
    // Every persona file — including the shipped defaults — is empty. FR-16
    // per-file seeding SKIPS files that already exist (even empty ones), so
    // it doesn't refill them, and the listing is genuinely empty → (none).
    for f in ["coder.md", "coach.md", "personal-assistant.md", "blank.md"] {
        fs::write(dir.join(f), "").unwrap();
    }
    let store = PersonaStore::new(dir);
    let msg = store.list_message().unwrap();
    assert!(msg.contains("(none)"), "got: {msg}");
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn handle_persona_command_show_and_clear() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_str().unwrap();
    let store = PersonaStore::new(tmp.path().join("personas"));
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
    let mut active = Some(test_persona(
        "terse",
        "Short.",
        tmp.path().join("personas").join("terse.md"),
    ));
    let mut system = rebuild_system_prompt(workspace, &memory, active.as_ref(), "test-session");
    let mut active_conversation_id = String::from("test-session");
    let mode_states = ConversationModeStates::default();

    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    // show: reports the active persona, does not reset anything.
    let msg = {
        let mut ctx = ConversationResetContext {
            memory: &mut memory,
            system: &mut system,
            conversation_id: &mut active_conversation_id,
            mode_states: &mode_states,
        };
        handle_persona_command("/persona show", workspace, &store, &mut active, &mut ctx).unwrap()
    };
    assert!(msg.contains("Active persona: terse"));
    assert!(active.is_some(), "show must not clear the persona");

    // clear: drops the persona and starts a fresh conversation.
    let msg = {
        let mut ctx = ConversationResetContext {
            memory: &mut memory,
            system: &mut system,
            conversation_id: &mut active_conversation_id,
            mode_states: &mode_states,
        };
        handle_persona_command("/persona clear", workspace, &store, &mut active, &mut ctx).unwrap()
    };
    assert_eq!(msg, "Started a new conversation with no active persona.");
    assert!(active.is_none());
    assert!(!system.contains("Active persona: terse"));
    let messages = memory.build_messages(&system, "new task");
    assert!(!messages.iter().any(|m| m.content == "old task"));
}

/// Writing a role-bound persona file and loading it must surface the
/// front-matter (role/tools/caveats), and a swap must change more than the
/// prompt versus the prompt-only `coder` default.
#[serial_test::serial(real_fs)]
#[test]
fn role_bound_persona_loads_tools_and_caveats() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("wing-commander.md"),
            "+++\nrole = \"wing-commander\"\ntools = [\"read_file\", \"grade_diff\"]\ntier = \"REVIEW\"\n\n[caveats]\nfs_write = \"none\"\nmax_calls = 60\n+++\n\n# Wing-Commander\nGrade diffs.\n",
        )
        .unwrap();
    let store = PersonaStore::new(dir);

    let wc = store.load("wing-commander").unwrap();
    assert_eq!(wc.profile.role.as_deref(), Some("wing-commander"));
    assert_eq!(wc.profile.tier, Some(newt_core::Tier::Review));
    assert_eq!(
        wc.profile.tools.as_deref(),
        Some(["read_file".to_string(), "grade_diff".to_string()].as_slice())
    );
    // Front-matter must NOT leak into the injected prompt.
    assert!(!wc.prompt.contains("+++"));
    assert!(wc.prompt.contains("Grade diffs."));
    let caveats = wc.profile.caveats.as_ref().unwrap().to_caveats();
    assert_eq!(caveats.fs_write, newt_core::Scope::none());
    assert_eq!(caveats.max_calls, newt_core::CountBound::AtMost(60));

    // The built-in `coder` default is prompt-only — a swap to
    // wing-commander changes MORE than the prompt. Parse the default soul
    // directly (the temp dir already has a persona file, so the `coder`
    // default isn't seeded into it).
    let coder = newt_core::RoleProfile::parse(newt_core::DEFAULT_SOUL).unwrap();
    assert!(!coder.is_role_bound());
    assert!(wc.profile.is_role_bound());
    assert_ne!(wc.profile.tools, coder.tools);
    assert_ne!(wc.profile.caveats, coder.caveats);
}

/// FR-1 (#997): a persona's read-only `[caveats]` are ENFORCED — met into the
/// turn authority so they can only TIGHTEN it, never widen the session grant.
#[serial_test::serial(real_fs)]
#[test]
fn persona_read_only_caveats_tighten_the_turn_authority() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("coach.md"),
            "+++\nrole = \"coach\"\n\n[caveats]\nfs_write = \"none\"\nexec = \"none\"\n+++\n\n# Coach\nRead-only.\n",
        )
        .unwrap();
    let coach = PersonaStore::new(dir).load("coach").unwrap();
    let full = newt_core::Caveats {
        fs_read: newt_core::Scope::All,
        fs_write: newt_core::Scope::All,
        exec: newt_core::Scope::All,
        net: newt_core::Scope::All,
        max_calls: newt_core::CountBound::Unlimited,
        valid_for_generation: newt_core::Scope::All,
    };
    // With the persona: fs_write + exec drop to none; read is untouched.
    let met = super::meet_persona_caveats(full.clone(), Some(&coach));
    assert_eq!(
        met.fs_write,
        newt_core::Scope::none(),
        "read-only persona drops fs_write"
    );
    assert_eq!(
        met.exec,
        newt_core::Scope::none(),
        "read-only persona drops exec"
    );
    assert_eq!(
        met.fs_read,
        newt_core::Scope::All,
        "read authority unchanged"
    );
    // No persona: the authority is unchanged.
    assert_eq!(
        super::meet_persona_caveats(full, None).fs_write,
        newt_core::Scope::All
    );
}

/// `/persona set <name> --keep-context` swaps the role WITHOUT discarding
/// conversation history (persistent-actor principle); the default resets.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn persona_set_keep_context_preserves_history() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_str().unwrap();
    let dir = tmp.path().join("personas");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("terse.md"), "Keep it short.").unwrap();
    let store = PersonaStore::new(dir);
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
    let mut active: Option<Persona> = None;
    let mut system = rebuild_system_prompt(workspace, &memory, active.as_ref(), "test-session");
    let mut active_conversation_id = String::from("test-session");
    let mode_states = ConversationModeStates::default();

    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let msg = {
        let mut ctx = ConversationResetContext {
            memory: &mut memory,
            system: &mut system,
            conversation_id: &mut active_conversation_id,
            mode_states: &mode_states,
        };
        handle_persona_command(
            "/persona set terse --keep-context",
            workspace,
            &store,
            &mut active,
            &mut ctx,
        )
        .unwrap()
    };
    assert!(msg.contains("kept conversation context"), "got: {msg}");
    assert_eq!(active.as_ref().unwrap().name, "terse");
    // History survives the swap.
    let messages = memory.build_messages(&system, "new task");
    assert!(
        messages.iter().any(|m| m.content == "old task"),
        "keep-context must preserve prior turns"
    );

    // Without the flag, the same swap resets the conversation.
    {
        let mut ctx = ConversationResetContext {
            memory: &mut memory,
            system: &mut system,
            conversation_id: &mut active_conversation_id,
            mode_states: &mode_states,
        };
        handle_persona_command(
            "/persona set terse",
            workspace,
            &store,
            &mut active,
            &mut ctx,
        )
        .unwrap();
    }
    let messages = memory.build_messages(&system, "new task");
    assert!(
        !messages.iter().any(|m| m.content == "old task"),
        "default swap must reset the conversation"
    );
}

/// All shipped role templates under `<repo>/personas/` parse into valid,
/// role-bound `RoleProfile`s with distinct tool sets (incl. the FR-16
/// coach and the #1021 personal-assistant).
#[test]
fn shipped_role_templates_parse() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-tui is a workspace member");
    let personas = repo_root.join("personas");
    for name in [
        "dragon-rider",
        "wing-commander",
        "worker",
        "coach",
        "personal-assistant",
    ] {
        let path = personas.join(format!("{name}.md"));
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing shipped template {}: {e}", path.display()));
        let rp = newt_core::RoleProfile::parse(&raw)
            .unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));
        assert_eq!(rp.role.as_deref(), Some(name), "{name} role mismatch");
        assert!(rp.is_role_bound(), "{name} must be role-bound");
        assert!(rp.tools.is_some(), "{name} must declare tools");
        assert!(rp.caveats.is_some(), "{name} must declare caveats");
        // Converts to canonical caveats without panicking.
        let _ = rp.caveats.unwrap().to_caveats();
    }
}

/// #1021 FR-PA-3/FR-PA-4: the shipped `personal-assistant` persona binds
/// the `gila-personal-assistant` skill (FR-4, #1041) and its `tools:`
/// allow-list is exactly its modulex MCP tools plus the infra tools the
/// agentic loop needs every round — nothing else. FR-PA-4 itself (persona
/// tool-allowlist filtering) needed no new code: `filter_advertised_tools`
/// / `persona_tool_allowed` (newt-core's `agentic::tools`) already
/// enforce this generically, covered by their own existing test suite
/// (e.g. `persona_tool_allowed_admits_named_and_always_on_only`); this
/// test only asserts the persona's *data* is what FR-PA-4 depends on.
#[test]
fn personal_assistant_persona_binds_gila_skill_and_modulex_tools_only() {
    let rp = newt_core::RoleProfile::parse(PERSONAL_ASSISTANT_PERSONA).unwrap();
    assert_eq!(
        rp.skills,
        Some(vec!["gila-personal-assistant".to_string()]),
        "binds exactly the gila skill"
    );
    let tools = rp.tools.expect("personal-assistant must declare tools");
    for expected in ["modulex__routine_run", "modulex__report_get"] {
        assert!(
            tools.iter().any(|t| t == expected),
            "must advertise {expected}: {tools:?}"
        );
    }
    assert!(
        !tools
            .iter()
            .any(|t| t.starts_with("write_") || t == "run_command"),
        "must not advertise a mutating tool: {tools:?}"
    );
}
