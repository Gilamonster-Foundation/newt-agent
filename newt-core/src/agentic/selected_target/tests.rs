use super::super::scheduled::{execute_update_plan, plan_block, SessionStepLedger};
use super::*;

fn selection(path: &str) -> Value {
    json!({"path":path,"scope":"repository implementation files",
        "evidence":"inventory ranked this source first","revision":"Initial selection"})
}
fn pin(ledger: &dyn StepLedger, path: &str) {
    let out = execute_update_plan(
        &json!({"plan":[{"step":"Extract"},{"step":"Check"}],
        "target":selection(path)}),
        ledger,
        false,
        0,
    );
    assert!(!out.starts_with("error:"), "{out}");
}

#[test]
fn selected_target_revision_is_explicit_linked_and_omission_preserves_it() {
    let ledger = SessionStepLedger::default();
    pin(&ledger, "alpha/nested/index.py");
    let before = ledger.snapshot().target.unwrap();
    let mut revised = selection("beta/nested/entry.ts");
    revised.as_object_mut().unwrap().remove("revision");
    let err = execute_update_plan(
        &json!({"plan":["Replan"],"target":revised}),
        &ledger,
        false,
        0,
    );
    assert!(err.starts_with("error:"), "{err}");
    assert_eq!(ledger.snapshot().target.as_ref(), Some(&before));
    revised["revision"] = json!("Operator changed scope to beta; fresh inventory");
    let out = execute_update_plan(
        &json!({"plan":["Replan"],"target":revised}),
        &ledger,
        false,
        0,
    );
    assert!(out.contains("beta/nested/entry.ts"), "{out}");
    let after = ledger.snapshot().target.unwrap();
    assert_ne!(after.id, before.id);
    assert_eq!(after.selection.previous, Some(before.id));
    execute_update_plan(&json!({"plan":["Finish"]}), &ledger, false, 0);
    assert_eq!(ledger.snapshot().target, Some(after));
    ledger.clear();
    assert!(ledger.snapshot().target.is_none());
}

#[test]
fn selected_target_paths_are_portable_and_identity_is_verified() {
    for path in [
        "../escape",
        "/absolute",
        "C:/drive",
        "a\\b",
        "a//b",
        "./a",
        "a/../b",
        "a\nb",
    ] {
        assert!(
            PinnedTarget::revise(&selection(path), None).is_err(),
            "{path}"
        );
    }
    for path in [
        "a/nested/index.py",
        "engine/module/root.code",
        "single-file",
        "目录/文件.txt",
    ] {
        assert!(PinnedTarget::revise(&selection(path), None)
            .unwrap()
            .valid());
    }
    let mut target = PinnedTarget::revise(&selection("a/index.py"), None).unwrap();
    target.selection.path = "wrong/index.py".into();
    assert!(!target.valid());
    assert!(target.label().contains("INVALID PIN"));
    assert!(card(&target, Some(&["wrong/index.py".into()])).contains("identity mismatch"));
}

#[test]
fn selected_target_wrong_crate_enoent_does_not_reselect_and_diff_is_advisory() {
    let target = PinnedTarget::revise(&selection("engine/nested/root.code"), None).unwrap();
    let outside = vec!["cli/nested/root.code".into()];
    let warning = card(&target, Some(&outside));
    assert!(warning.contains("work landed outside the selected source"));
    assert!(warning.contains("engine/nested/root.code"));
    assert!(warning.contains("ENOENT"));
    assert!(card(&target, None).contains("unverified, not a pass"));
    assert!(card(&target, Some(&[])).contains("No task diff"));
    assert!(!card(
        &target,
        Some(&[target.selection.path.clone(), "new/component.code".into()])
    )
    .contains("work landed outside"));
}

#[test]
fn selected_target_projection_survives_compaction_continue_and_done_plan() {
    let ledger = SessionStepLedger::default();
    pin(&ledger, "arbitrary/nested/root.xyz");
    execute_update_plan(
        &json!({"plan":[{"step":"Done","status":"completed"}]}),
        &ledger,
        false,
        0,
    );
    let mut messages = vec![json!({"role":"user","content":"continue"})];
    super::super::prompt_read::ensure_active_prompt_card(
        &mut messages,
        super::super::prompt_read::PromptReadContext::new(None, "Original task", None),
        None,
    );
    let mut projection = Projection::default();
    projection.refresh(
        &mut messages,
        Some(&ledger),
        ".",
        &crate::Scope::none(),
        None,
        false,
    );
    for n in 0..20 {
        messages.push(json!({"role":"user","content":format!("noise {n}")}));
    }
    let head = super::super::trim::protected_prompt_head_len(
        &messages,
        super::super::prompt_read::ACTIVE_PROMPT_PREFIX,
    );
    let mut trimmed = super::super::trim::trim_for_summary(&messages, head, 2);
    assert!(trimmed.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("arbitrary/nested/root.xyz"))));
    projection.refresh(
        &mut trimmed,
        Some(&ledger),
        ".",
        &crate::Scope::none(),
        None,
        false,
    );
    assert_eq!(
        trimmed
            .iter()
            .filter(|m| m["content"].as_str().is_some_and(|s| s.starts_with(PREFIX)))
            .count(),
        1
    );
    assert!(plan_block(&ledger)
        .unwrap()
        .contains("arbitrary/nested/root.xyz"));
    // Responses compaction temporarily reconstructs the same protected head.
    let mut input = vec![json!({"role":"user","content":"continue"})];
    Projection::default().refresh(
        &mut input,
        Some(&ledger),
        ".",
        &crate::Scope::none(),
        None,
        true,
    );
    let protected =
        super::super::compress::protect_active_prompt_for_compression(&input, "Original task");
    let head = super::super::trim::protected_prompt_head_len(
        &protected,
        super::super::prompt_read::ACTIVE_PROMPT_PREFIX,
    );
    assert!(protected[..head]
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|s| s.starts_with(PREFIX))));
}

/// Grounds the pure path reconciliation in real local Git, with no network.
#[test]
fn selected_target_git_diff_includes_commits_dirty_untracked_and_source_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let run = |args: &[&str]| {
        let out = crate::agentic::tools::git_fixture::hermetic_git(root, root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Fixture"]);
    run(&["config", "user.email", "fixture@example.invalid"]);
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/source.txt"), "original").unwrap();
    std::fs::write(root.join("other.txt"), "other").unwrap();
    run(&["add", "."]);
    run(&["-c", "commit.gpgsign=false", "commit", "-qm", "base"]);
    let mut target = PinnedTarget::revise(&selection("nested/source.txt"), None).unwrap();
    target
        .bind_baseline(baseline(root, &crate::Scope::All))
        .unwrap();
    std::fs::write(root.join("other.txt"), "changed").unwrap();
    run(&["add", "."]);
    run(&["-c", "commit.gpgsign=false", "commit", "-qm", "outside"]);
    let paths = diff_paths(&target, root, &crate::Scope::All).unwrap();
    assert!(card(&target, Some(&paths)).contains("work landed outside"));
    std::fs::write(root.join("untracked.txt"), "new").unwrap();
    assert!(diff_paths(&target, root, &crate::Scope::All)
        .unwrap()
        .contains(&"untracked.txt".into()));
    run(&["mv", "nested/source.txt", "nested/moved.txt"]);
    let paths = diff_paths(&target, root, &crate::Scope::All).unwrap();
    assert!(paths.contains(&"nested/source.txt".into()));
    assert!(!card(&target, Some(&paths)).contains("work landed outside"));
    assert!(diff_paths(&target, root, &crate::Scope::none()).is_none());
}
