use super::*;

#[tokio::test]
async fn soul_provider_uses_default_when_no_file() {
    let mut sp = SoulProvider::new(None);
    let ctx = SessionContext {
        workspace: "/nonexistent".into(),
        session_id: "s".into(),
    };
    sp.initialize(&ctx).await.unwrap();
    assert_eq!(sp.source, SoulSource::Default);
    let block = sp.system_prompt_block().unwrap();
    assert!(block.contains("newt"), "default soul should mention newt");
}
#[test]
fn default_soul_lists_all_current_tools() {
    // Regression: DEFAULT_SOUL went stale when use_skill (#135) and
    // web_fetch (#139) were added but the constant wasn't updated, so
    // default-identity sessions never learned those tools existed.
    // find (#496) is the latest such addition; render_report (#1004) is
    // the newest.
    for tool in [
        "run_command",
        "read_file",
        "write_file",
        "edit_file",
        "list_dir",
        "find",
        "use_skill",
        "web_fetch",
        "render_report",
    ] {
        assert!(
            DEFAULT_SOUL.contains(tool),
            "DEFAULT_SOUL must advertise `{tool}`"
        );
    }
}
/// FR-5 (#999) golden contract: the coach identity must instruct the model
/// to ADVISE, not act — name the mutating tools as things NOT to call, frame
/// the turn as advising, and drop the doer's imperative. This is the
/// deterministic essence of "a scripted incident turn emits no mutating tool
/// call": the model's behavior follows from this directive, so if COACH_SOUL
/// ever loses it the coach silently regresses into a doer.
#[test]
fn coach_soul_forbids_mutation_and_mandates_advice() {
    // Names the mutating tools it must not call.
    for mutating in ["write_file", "edit_file", "run_command"] {
        assert!(
            COACH_SOUL.contains(mutating),
            "COACH_SOUL must name `{mutating}` (as forbidden)"
        );
    }
    assert!(
        COACH_SOUL.contains("Do not call write_file"),
        "COACH_SOUL must carry the explicit no-mutation directive"
    );
    assert!(
        COACH_SOUL.to_lowercase().contains("advis"),
        "COACH_SOUL must frame the turn as advising"
    );
    // And it must NOT carry the doer's act-first imperative.
    assert!(
        !COACH_SOUL.contains("Never describe a code change — make it"),
        "COACH_SOUL must not inherit the doer imperative"
    );
}
#[tokio::test]
async fn soul_provider_loads_workspace_soul() {
    let dir = tempfile::tempdir().unwrap();
    // Create .newt/soul.md inside the temp dir.
    let newt_dir = dir.path().join(".newt");
    std::fs::create_dir_all(&newt_dir).unwrap();
    std::fs::write(newt_dir.join("soul.md"), "You are a Django expert.").unwrap();

    let mut sp = SoulProvider::new(None);
    let ctx = SessionContext {
        workspace: dir.path().to_string_lossy().into(),
        session_id: "s".into(),
    };
    sp.initialize(&ctx).await.unwrap();
    assert_eq!(sp.source, SoulSource::Workspace);
    let block = sp.system_prompt_block().unwrap();
    assert!(block.contains("Django"), "should use workspace soul");
}
#[tokio::test]
async fn soul_provider_explicit_path_wins() {
    let dir = tempfile::tempdir().unwrap();
    let soul_file = dir.path().join("custom_soul.md");
    std::fs::write(&soul_file, "You are a security auditor.").unwrap();

    // Also create a workspace soul — explicit should win.
    let ws_dir = tempfile::tempdir().unwrap();
    let newt_dir = ws_dir.path().join(".newt");
    std::fs::create_dir_all(&newt_dir).unwrap();
    std::fs::write(newt_dir.join("soul.md"), "You are a Django expert.").unwrap();

    let mut sp = SoulProvider::new(Some(soul_file.clone()));
    let ctx = SessionContext {
        workspace: ws_dir.path().to_string_lossy().into(),
        session_id: "s".into(),
    };
    sp.initialize(&ctx).await.unwrap();
    assert_eq!(sp.source, SoulSource::Explicit(soul_file));
    let block = sp.system_prompt_block().unwrap();
    assert!(
        block.contains("security auditor"),
        "explicit path should win"
    );
}
#[tokio::test]
async fn soul_provider_empty_workspace_soul_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let newt_dir = dir.path().join(".newt");
    std::fs::create_dir_all(&newt_dir).unwrap();
    // Empty workspace soul → should fall through to default.
    std::fs::write(newt_dir.join("soul.md"), "   ").unwrap();

    let mut sp = SoulProvider::new(None);
    let ctx = SessionContext {
        workspace: dir.path().to_string_lossy().into(),
        session_id: "s".into(),
    };
    sp.initialize(&ctx).await.unwrap();
    // Empty file → falls through to default.
    assert_eq!(sp.source, SoulSource::Default);
}
