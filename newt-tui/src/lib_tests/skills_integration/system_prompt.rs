use super::*;

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
