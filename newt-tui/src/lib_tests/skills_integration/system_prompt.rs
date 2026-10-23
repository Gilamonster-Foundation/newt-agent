use super::*;

pub(super) fn write_skill(root: &std::path::Path, name: &str, desc: &str) {
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

/// #2331: field evidence showed a model asked to install a skill it had never
/// installed produced zero tool calls and promised to investigate instead,
/// because it had never been told a real install command exists. The block is
/// no longer `None` with zero skills installed — it still carries the install
/// hint, which is the one thing the model needs to acquire the first one.
#[serial_test::serial(real_fs)]
#[test]
fn system_prompt_index_carries_the_install_hint_when_no_skills() {
    let tmp = tempfile::TempDir::new().unwrap();
    let block =
        skills_index_for_prompt(&[tmp.path().to_path_buf()]).expect("install hint survives");
    assert!(block.contains("newt skills install"));
    assert!(!block.contains("Available skills"));
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

/// #2935: the identity comes from the `SoulProvider` the runtime always
/// installs (`chat.rs`), so the prompt builder no longer carries its own
/// `DEFAULT_SOUL` fallback. A memory manager with no soul provider yields a
/// prompt with no soul — the builder never invents one.
///
/// Red before the removal: the `None` soul arm injected DEFAULT_SOUL.
#[serial_test::serial(real_fs)]
#[test]
fn no_soul_provider_means_no_default_soul_2935() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let tmp = tempfile::TempDir::new().unwrap();
    let memory = newt_core::MemoryManager::new();
    let prompt = rebuild_system_prompt(tmp.path().to_str().unwrap(), &memory, None, "s", None);
    let first_line = newt_core::DEFAULT_SOUL
        .lines()
        .find(|l| !l.is_empty())
        .unwrap();
    assert!(
        !prompt.contains(first_line),
        "the builder must not inject DEFAULT_SOUL on its own; got: {prompt:.200}"
    );
    assert!(
        prompt.contains("Workspace: "),
        "the rest of the prompt is intact"
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
    let prompt_a =
        build_system_prompt_with_persona(ws, newt_core::DEFAULT_SOUL, None, None, &path_a, None);
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
    let prompt_b = build_system_prompt_with_persona(
        ws,
        newt_core::DEFAULT_SOUL,
        None,
        None,
        &path_b.to_string_lossy(),
        None,
    );
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
    let prompt = build_system_prompt_with_persona(
        tmp.path().to_str().unwrap(),
        "You are a custom agent.",
        None,
        None,
        ".scratch/sessions/xyz/plan.md",
        None,
    );
    assert!(prompt.contains("You are a custom agent."));
    assert!(prompt.contains(".scratch/sessions/xyz/plan.md"));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn registered_agents_provider_block_reaches_prompt() {
    // rebuild_system_prompt below republishes process-global runtime
    // settings as a side effect (Config::resolve().publish_runtime_settings());
    // hold the guard for the whole test, not just its assertions.
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
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
        None,
    );
    assert!(prompt.contains("# Project instructions"));
    assert!(prompt.contains("Run just check before PRs."));
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn historical_notes_preserve_content_without_redefining_the_live_workspace() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::TempDir::new().unwrap();
    let old_workspace = dir.path().join("previous-repository");
    let workspace = dir.path().join("current-repository");
    fs::create_dir_all(&old_workspace).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let note = format!(
        "The workspace root is {} on branch previous-work; use the retired_git tool.",
        old_workspace.display()
    );
    let notes_path = dir.path().join("NOTES.md");
    let original_notes = format!("{note}\n§\n");
    fs::write(&notes_path, &original_notes).unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::NoteStore::new(&notes_path, 2200));
    let ctx = newt_core::SessionContext {
        workspace: workspace.to_string_lossy().into_owned(),
        session_id: "current-session".into(),
    };
    memory.initialize_all(&ctx).await;
    let prompt = rebuild_system_prompt(&ctx.workspace, &memory, None, &ctx.session_id, None);

    assert_eq!(fs::read_to_string(&notes_path).unwrap(), original_notes);
    assert_eq!(prompt.matches(&note).count(), 1, "retain the note verbatim");
    let note_position = prompt.find(&note).unwrap();
    let history_position = prompt
        .find("Historical notes from prior sessions")
        .expect("cross-session notes must be framed as historical context");
    assert!(history_position < note_position);
    assert!(prompt.contains("Workspace, branch, and tool availability may be stale"));
    assert!(prompt.contains("do not redefine current runtime state"));
    let live_workspace_position = prompt
        .find(&format!("Workspace: {}", workspace.display()))
        .expect("the prompt must identify the actual initialized workspace");
    assert!(note_position < live_workspace_position);
    let live_context = &prompt[live_workspace_position..];
    assert!(live_context.contains("current runtime root"));
    assert!(live_context.contains("Relative tool paths resolve here"));
    assert!(live_context
        .contains("run_command starts here unless its cwd or command selects another directory"));
    assert!(
        live_context.contains("Other requested paths remain governed by the session's permissions")
    );
}

/// Pins the process-wide inputs skill resolution reads — `NEWT_CONFIG`,
/// `NEWT_CONFIG_DIR`, `HOME` and the cwd — restoring them on drop, so a
/// failing assertion cannot leak a redirected root into the next test.
pub(super) struct SkillRootSandbox {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    cwd: std::path::PathBuf,
}

impl SkillRootSandbox {
    pub(super) fn enter(config_dir: &std::path::Path, cwd: &std::path::Path) -> Self {
        let keys = [
            "NEWT_CONFIG",
            newt_core::config::NEWT_CONFIG_DIR_ENV,
            "HOME",
        ];
        let sandbox = Self {
            vars: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            cwd: std::env::current_dir().unwrap(),
        };
        // SAFETY: the caller is in the `serial(real_fs)` lane.
        unsafe {
            std::env::remove_var("NEWT_CONFIG");
            std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, config_dir);
            std::env::set_var("HOME", config_dir);
        }
        std::env::set_current_dir(cwd).unwrap();
        sandbox
    }
}

impl Drop for SkillRootSandbox {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.cwd);
        for (key, value) in &self.vars {
            // SAFETY: still inside the `serial(real_fs)` lane.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// #2331 real-resource grounding for the mocked resolver and same-copy tests
/// (`skill_search_dirs_append_the_checkout_bundled_default_when_unset` in
/// newt-core, `index_and_loader_resolve_the_same_copy_over_user_then_bundled`
/// in newt-skills): on a real filesystem, a skill that exists ONLY in a
/// checkout's `.newt/bundled-skills` is listed by the real system prompt AND
/// loaded by the real `use_skill` dispatch. Before #2331 the prompt listed it
/// and `use_skill` answered `unknown skill`. Asserting the index alone would
/// pass on that bug, so both halves are required.
#[ignore = "real-resource: weekly/release tier; pins cwd, HOME and NEWT_CONFIG_DIR"]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn a_bundled_only_skill_is_indexed_and_loaded_by_use_skill() {
    let root = tempfile::TempDir::new().unwrap();
    let checkout = root.path().join("checkout");
    write_skill(
        &checkout.join(".newt").join("bundled-skills"),
        "grounding-skill",
        "Only in the bundled dir",
    );
    let nested = checkout.join("crate");
    let config_dir = root.path().join("config");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    let _sandbox = SkillRootSandbox::enter(&config_dir, &nested);
    let ws = nested.to_str().unwrap();

    let prompt =
        build_system_prompt_with_persona(ws, newt_core::DEFAULT_SOUL, None, None, "plan.md", None);
    assert!(
        prompt.contains("grounding-skill: Only in the bundled dir"),
        "the prompt index must list the bundled-only skill"
    );

    let body = newt_core::execute_tool(
        "use_skill",
        &serde_json::json!({ "name": "grounding-skill" }),
        ws,
        false,
        200,
        &newt_core::Caveats::top(),
        &mut newt_core::NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        body.contains("Full body of grounding-skill."),
        "use_skill must load the copy the index listed, got: {body}"
    );
}

// #2937: workspace-header annotation must not swallow project-context provider
// annotations. Doer: soul + agents + notes providers all get individual labels;
// workspace-header covers only the workspace line, not provider content.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn provider_annotations_labeled_independently_doer_2937() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = dir.path();
    fs::write(ws.join("AGENTS.md"), "Use four spaces for indentation.").unwrap();
    let notes_path = ws.join("NOTES.md");
    fs::write(&notes_path, "user: prefers vi\n§\n").unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::SoulProvider::new(None));
    memory.add_provider(newt_core::AgentsProvider::new(true, None));
    memory.add_provider(newt_core::NoteStore::new(&notes_path, 2200));
    let ctx = newt_core::SessionContext {
        workspace: ws.to_string_lossy().into_owned(),
        session_id: "s".into(),
    };
    memory.initialize_all(&ctx).await;

    let sink = newt_core::agentic::request_report::ReportSink::new();
    let prompt = rebuild_system_prompt(
        ws.to_str().unwrap(),
        &memory,
        None,
        "sess-doer",
        Some(&sink),
    );

    sink.arm_capture_next();
    let _ = sink.observe_request(&[&prompt], None);
    let report = sink.last_report().expect("report captured");
    let labels: Vec<&str> = report.system[0]
        .components
        .iter()
        .map(|c| c.label.as_str())
        .collect();

    // #2933: the doer soul is reported as its components, by id.
    assert!(
        labels.contains(&"identity") && labels.contains(&"habit-one-change"),
        "doer soul must be labelled by component id; got {labels:?}"
    );
    assert!(
        !labels.contains(&"soul"),
        "no whole-soul label; got {labels:?}"
    );
    assert!(
        labels.contains(&"project-instructions"),
        "'project-instructions' (agents) provider must be labeled; got {labels:?}"
    );
    assert!(
        labels.contains(&"notes"),
        "'notes' (note_store) provider must be labeled; got {labels:?}"
    );
    assert!(
        labels.contains(&"workspace-header"),
        "workspace-header must be labeled; got {labels:?}"
    );

    // workspace-header must be a separate, short component — not one that swallows
    // provider blocks. The workspace header text is ~200 chars; provider content
    // adds thousands. A swallowing annotation would have chars >> 500.
    let ws_chars = report.system[0]
        .components
        .iter()
        .find(|c| c.label == "workspace-header")
        .expect("workspace-header component must exist")
        .chars;
    assert!(
        ws_chars < 500,
        "workspace-header chars ({ws_chars}) must be small — not absorbing provider blocks"
    );
}

// #2937: coach altitude — workspace-header must not swallow context providers.
// Coach replaces soul with COACH_SOUL, but agents and notes remain labeled.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn provider_annotations_labeled_independently_coach_2937() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = dir.path();
    fs::write(ws.join("AGENTS.md"), "Always ask before acting.").unwrap();
    let notes_path = ws.join("NOTES.md");
    fs::write(&notes_path, "project: uses just check\n§\n").unwrap();

    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::SoulProvider::new(None));
    memory.add_provider(newt_core::AgentsProvider::new(true, None));
    memory.add_provider(newt_core::NoteStore::new(&notes_path, 2200));
    let ctx = newt_core::SessionContext {
        workspace: ws.to_string_lossy().into_owned(),
        session_id: "s".into(),
    };
    memory.initialize_all(&ctx).await;

    let mut coach_persona = test_persona(
        "coach",
        "Give advice.",
        std::path::PathBuf::from("/personas/coach.md"),
    );
    coach_persona.profile.altitude = Some(newt_core::Altitude::Coach);

    let sink = newt_core::agentic::request_report::ReportSink::new();
    let prompt = rebuild_system_prompt(
        ws.to_str().unwrap(),
        &memory,
        Some(&coach_persona),
        "sess-coach",
        Some(&sink),
    );

    sink.arm_capture_next();
    let _ = sink.observe_request(&[&prompt], None);
    let report = sink.last_report().expect("report captured");
    let labels: Vec<&str> = report.system[0]
        .components
        .iter()
        .map(|c| c.label.as_str())
        .collect();

    assert!(
        labels.contains(&"coach-soul"),
        "coach must have 'coach-soul' label; got {labels:?}"
    );
    assert!(
        labels.contains(&"project-instructions"),
        "'project-instructions' (agents) provider must be labeled; got {labels:?}"
    );
    assert!(
        labels.contains(&"notes"),
        "'notes' (note_store) provider must be labeled; got {labels:?}"
    );
    assert!(
        labels.contains(&"workspace-header"),
        "workspace-header must be labeled; got {labels:?}"
    );

    let ws_chars = report.system[0]
        .components
        .iter()
        .find(|c| c.label == "workspace-header")
        .expect("workspace-header component must exist")
        .chars;
    assert!(
        ws_chars < 500,
        "workspace-header chars ({ws_chars}) must not absorb provider blocks"
    );

    // Prompt must contain AGENTS.md and notes content despite coach altitude.
    assert!(
        prompt.contains("Always ask before acting"),
        "agents content must reach prompt"
    );
    assert!(
        prompt.contains("uses just check"),
        "notes content must reach prompt"
    );
}

// #2937: when no soul provider is present, context providers (agents, notes)
// still receive individual annotations. No DEFAULT_SOUL must be invented.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn no_soul_provider_context_providers_still_annotated_2937() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = dir.path();
    fs::write(ws.join("AGENTS.md"), "Commit with clear messages.").unwrap();
    let notes_path = ws.join("NOTES.md");
    fs::write(&notes_path, "user: uses rg not grep\n§\n").unwrap();

    // No SoulProvider — soul_block() returns None.
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::AgentsProvider::new(true, None));
    memory.add_provider(newt_core::NoteStore::new(&notes_path, 2200));
    let ctx = newt_core::SessionContext {
        workspace: ws.to_string_lossy().into_owned(),
        session_id: "s".into(),
    };
    memory.initialize_all(&ctx).await;

    let sink = newt_core::agentic::request_report::ReportSink::new();
    let prompt = rebuild_system_prompt(
        ws.to_str().unwrap(),
        &memory,
        None,
        "sess-nosoul",
        Some(&sink),
    );

    // No DEFAULT_SOUL fallback — the builder never invents one (#2935).
    let first_line = newt_core::DEFAULT_SOUL
        .lines()
        .find(|l| !l.is_empty())
        .unwrap();
    assert!(
        !prompt.contains(first_line),
        "builder must not inject DEFAULT_SOUL; got: {prompt:.200}"
    );

    sink.arm_capture_next();
    let _ = sink.observe_request(&[&prompt], None);
    let report = sink.last_report().expect("report captured");
    let labels: Vec<&str> = report.system[0]
        .components
        .iter()
        .map(|c| c.label.as_str())
        .collect();

    assert!(
        labels.contains(&"project-instructions"),
        "'project-instructions' must be labeled even with no soul; got {labels:?}"
    );
    assert!(
        labels.contains(&"notes"),
        "'note_store' must be labeled even with no soul; got {labels:?}"
    );
    assert!(
        !labels.contains(&"soul"),
        "no soul provider means no 'soul' label; got {labels:?}"
    );
}

/// #2938: with a system prompt file set, the session's prompt is that file's
/// bytes and nothing of the assembled prompt; unset, it is assembled again.
/// Grounds the mocked override tests in a real file read.
#[serial_test::serial(real_fs)]
#[test]
fn a_system_prompt_file_replaces_the_assembled_prompt_2938() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().to_str().unwrap();
    let file = tmp.path().join("pruned.md");
    let pruned = "Be brief.\n\n  Keep this spacing.\n";
    std::fs::write(&file, pruned).unwrap();
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::SoulProvider::new(None));

    newt_core::process_env::set_var(newt_core::SYSTEM_PROMPT_FILE_ENV, file.to_str().unwrap());
    let overridden = rebuild_system_prompt(workspace, &memory, None, "s", None);
    newt_core::process_env::set_var(newt_core::SYSTEM_PROMPT_FILE_ENV, "");
    let assembled = rebuild_system_prompt(workspace, &memory, None, "s", None);
    newt_core::process_env::remove_var(newt_core::SYSTEM_PROMPT_FILE_ENV);

    assert_eq!(overridden, pruned);
    assert!(
        assembled.contains("Workspace: "),
        "unset, newt assembles its own: {assembled:.200}"
    );
}
