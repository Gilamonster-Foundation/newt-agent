use super::*;
use crate::agentic::{claim_check::TurnClaims, display::ToolPresentation};

fn git(root: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    command.env_clear().current_dir(root);
    for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let out = command
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-config"))
        .env("GIT_TEMPLATE_DIR", root.join("empty-template"))
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for path in ["src", "tests"] {
        std::fs::create_dir(dir.path().join(path)).unwrap();
    }
    std::fs::write(dir.path().join("src/mod.rs"), "one\ntwo\n").unwrap();
    std::fs::write(dir.path().join("tests/mod.rs"), "test\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);
    dir
}

/// Real Git grounds task-wide file facts even after a clean commit and continue.
#[test]
fn observed_report_retains_baseline_and_receipts_across_continues() {
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let session = crate::worktree_adoption::WorktreeSession::default();
    let first = TurnClaims::capture(workspace, &Scope::All, Some(&session));
    std::fs::write(
        dir.path().join("src/mod.rs"),
        "one\ntwo\nthree\nunterminated",
    )
    .unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "change"]);
    first.observe_report(
        workspace,
        &Scope::All,
        &Capture::default(),
        None,
        Some(&crate::git_staging::Outcome::PrCreated {
            url: "https://github.com/example/project/pull/7".into(),
        }),
    );
    drop(first);
    let next = TurnClaims::capture(workspace, &Scope::All, Some(&session));
    let text = next.observed_report(workspace, &Scope::All, "PR blocked. Changed mod.rs.");
    assert!(text.contains("`src/mod.rs`: 2 → 3"), "{text}");
    assert!(text.contains("PR creation observed:"), "{text}");
    assert!(text.contains("Unverified ambiguous bare paths"), "{text}");
    assert!(text.contains("`mod.rs`"), "{text}");
    let report = session.report.lock().unwrap();
    let (cid, bytes) = report.pinned.as_ref().unwrap();
    assert_eq!(*cid, ContentId::from_canonical_bytes(bytes));
}

/// A newly bound linked task owns its own adoption baseline and receipts.
#[test]
fn observed_report_adoption_binds_before_the_next_edit_and_lift_resets() {
    let dir = fixture();
    let task = dir.path().join("task");
    git(
        dir.path(),
        &["worktree", "add", "-qb", "task", task.to_str().unwrap()],
    );
    let session = crate::worktree_adoption::WorktreeSession::default();
    let workspace = dir.path().to_str().unwrap();
    let turn = TurnClaims::capture(workspace, &Scope::All, Some(&session));
    session.record_task_worktree(&task, "task");
    turn.observe_report(workspace, &Scope::All, &Capture::default(), None, None);
    std::fs::write(task.join("src/mod.rs"), "task\n").unwrap();
    let text = turn.observed_report(workspace, &Scope::All, "done");
    assert!(text.contains("`src/mod.rs`: 2 → 1"), "{text}");
    session.lift();
    assert!(session.report.lock().unwrap().roots.is_empty());
}

struct Quiet;
impl ToolPresentation for Quiet {
    fn preview(&mut self, _: &str, _: usize) {}
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

/// Numeric evidence is captured before terminal/model output cropping; last
/// exact-scope failure replaces success without erasing another package's check.
#[test]
fn observed_report_captures_bounded_results_and_last_exact_scope() {
    let dir = fixture();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    let capture = std::sync::Mutex::new(Capture::default());
    let mut quiet = Quiet;
    let mut view = Presentation {
        inner: &mut quiet,
        capture: Some(&capture),
    };
    for (command, code) in [
        ("cargo test -p a", 0),
        ("cargo check -p b", 0),
        ("cargo test -p a", 101),
    ] {
        view.execution_command(command, dir.path().to_str().unwrap());
        view.execution_result(&serde_json::json!({"exit_code":code,"stdout":"test result: ok. 3 passed; 0 failed\n", "stderr":"error: 1 target failed"}));
        state.observe(
            dir.path(),
            &capture.lock().unwrap(),
            Some(if code == 0 {
                ExecOutcome::Passed
            } else {
                ExecOutcome::Failed
            }),
            None,
        );
    }
    let text = state.render(dir.path(), &Scope::All, "done");
    assert_eq!(text.matches("`cargo test -p a`").count(), 1);
    assert!(text.contains("Failed; exit 101"), "{text}");
    assert!(text.contains("cargo check -p b"), "{text}");
    assert!(text.contains("test result: ok. 3 passed"), "{text}");
    assert!(text.contains("error: 1 target failed"), "{text}");
}

/// Unauthorized roots never produce fabricated zero counts, and a printed URL
/// or dry run never pays for a governed publication.
#[test]
fn observed_report_missing_evidence_is_explicit() {
    let dir = fixture();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::none());
    state.observe(
        dir.path(),
        &Capture::default(),
        Some(ExecOutcome::Passed),
        Some(&crate::git_staging::Outcome::DryRunChecked),
    );
    let text = state.render(dir.path(), &Scope::none(), "PR #7 created");
    assert!(text.contains("File counts unavailable"), "{text}");
    assert!(text.contains("No governed receipt available"), "{text}");
}

/// Only a new objective clears the retained facts; another turn of the same
/// objective retains them without depending on the latest user's wording.
#[test]
fn observed_report_objective_reset_and_root_isolation() {
    let dir = fixture();
    let other = fixture();
    let mut state = State::default();
    let objective = crate::prompt::PromptId::new();
    state.begin(objective);
    state.bind(dir.path(), &Scope::All);
    state.observe(
        dir.path(),
        &Capture::default(),
        None,
        Some(&crate::git_staging::Outcome::Pushed {
            oid: "abc123".into(),
            owner: "example".into(),
            name: "project".into(),
            branch: "task".into(),
        }),
    );
    state.begin(objective);
    assert!(state
        .render(dir.path(), &Scope::All, "done")
        .contains("Push observed:"));
    state.bind(other.path(), &Scope::All);
    assert!(!state
        .render(other.path(), &Scope::All, "done")
        .contains("Push observed:"));
    state.begin(crate::prompt::PromptId::new());
    assert!(state.roots.is_empty());
    assert!(state.pinned.is_none());
}

/// Same-LF edits still appear, new paths have unverified baselines, deletion
/// is verified, and a newline-free final line is not counted as an LF.
#[test]
fn observed_report_content_changes_not_just_line_deltas() {
    let dir = fixture();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    std::fs::write(dir.path().join("src/mod.rs"), "different\ncontent\n").unwrap();
    std::fs::remove_file(dir.path().join("tests/mod.rs")).unwrap();
    std::fs::write(dir.path().join("new.rs"), "no newline").unwrap();
    let text = state.render(dir.path(), &Scope::All, "changed mod.rs");
    assert!(text.contains("`src/mod.rs`: 2 → 2"), "{text}");
    assert!(
        text.contains("`new.rs`: unverified (not enumerated at baseline) → 0"),
        "{text}"
    );
    assert!(text.contains("`tests/mod.rs`: 1 → 0 (absent)"), "{text}");
    assert!(!text.contains("Unverified ambiguous bare paths"), "{text}");
}

/// Protocol assertions inspect model prose; report contracts are tested separately.
pub(crate) fn model_explanation(text: &str) -> &str {
    if text.starts_with("## Observed\n") {
        text.split_once("\n## Model explanation\n\n")
            .expect("observed report has an explanation boundary")
            .1
    } else {
        text
    }
}

#[path = "files_tests.rs"]
mod enumeration;

/// Slice 1: an actual edit replaces the pinned report; a pre-edit context
/// cannot be dispatched as if its facts were still the current observation.
#[test]
fn semantic_pins_refresh_actual_report_after_edit_and_refuse_stale_request() {
    let mut projection = crate::agentic::semantic_pins::Projection::default();
    use crate::agentic::{
        prompt_read::PromptReadContext, semantic_pins, smart_harness::SmartHarness,
    };
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(workspace, &Scope::All, None);
    let h = SmartHarness::new(
        agent_harness::Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
        std::sync::Arc::new(|_| Box::pin(async { anyhow::bail!("no inference in this test") })),
        Default::default(),
    )
    .unwrap();
    let mut messages = vec![serde_json::json!({"role":"user","content":"change source"})];
    let prompt = PromptReadContext::new(None, "change source", None);
    semantic_pins::refresh(
        &mut projection,
        Some(&h),
        &mut messages,
        prompt,
        None,
        &claims,
        workspace,
        &Scope::All,
    )
    .unwrap();
    let stale = messages.clone();
    std::fs::write(dir.path().join("src/mod.rs"), "new\nsource\nthree\n").unwrap();
    semantic_pins::refresh(
        &mut projection,
        Some(&h),
        &mut messages,
        prompt,
        None,
        &claims,
        workspace,
        &Scope::All,
    )
    .unwrap();
    assert_eq!(messages.len(), stale.len(), "old report must retire");
    assert_ne!(messages, stale);
    assert!(messages.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("`src/mod.rs`: 2 → 3"))));
    let sent = h
        .request(&serde_json::json!({"messages":messages}), "openai")
        .unwrap();
    assert!(String::from_utf8_lossy(&sent).contains("Historical checks retain their stated scope"));
    assert!(h
        .request(&serde_json::json!({"messages":stale}), "openai")
        .is_err());
}

/// Harness observations need their own labelled model message even without
/// smart navigation. A new check replaces the prior note, not the user task.
#[test]
fn observed_report_model_note_without_smart_harness() {
    let mut projection = crate::agentic::semantic_pins::Projection::default();
    use crate::agentic::{prompt_read::PromptReadContext, semantic_pins};
    let dir = fixture();
    let workspace = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(workspace, &Scope::All, None);
    let mut messages = vec![
        serde_json::json!({"role":"system", "content":crate::agentic::prompt_read::ACTIVE_PROMPT_PREFIX}),
        serde_json::json!({"role":"user","content":"continue"}),
    ];
    let prompt = PromptReadContext::new(None, "continue", None);
    let capture = Capture {
        command: Some(("cargo check".into(), workspace.into())),
        exit: Some(0),
        ..Capture::default()
    };
    claims.observe_report(
        workspace,
        &Scope::All,
        &capture,
        Some(ExecOutcome::Passed),
        None,
    );
    semantic_pins::refresh(
        &mut projection,
        None,
        &mut messages,
        prompt,
        None,
        &claims,
        workspace,
        &Scope::All,
    )
    .unwrap();
    let note = messages
        .iter()
        .find(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("[Harness observed facts"))
        })
        .expect("labelled harness note");
    assert_eq!(note["role"], "system");
    assert!(note["content"].as_str().unwrap().contains("cargo check"));
    assert!(note["content"]
        .as_str()
        .unwrap()
        .contains("not assistant-authored"));
    assert_eq!(
        crate::agentic::trim::protected_prompt_head_len(
            &messages,
            crate::agentic::prompt_read::ACTIVE_PROMPT_PREFIX
        ),
        messages.len(),
        "facts must not displace the exact task from the protected prompt head"
    );
    let original_len = messages.len();
    claims.observe_report(
        workspace,
        &Scope::All,
        &Capture {
            exit: Some(101),
            ..capture
        },
        Some(ExecOutcome::Failed),
        None,
    );
    semantic_pins::refresh(
        &mut projection,
        None,
        &mut messages,
        prompt,
        None,
        &claims,
        workspace,
        &Scope::All,
    )
    .unwrap();
    assert_eq!(messages.len(), original_len);
    assert_eq!(messages.last().unwrap()["content"], "continue");
    assert!(messages.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|s| s.contains("Failed; exit 101"))));
}

/// Harness notices are content-addressed report data, bounded and delimiter
/// safe. They never turn captured plan text into assistant-authored speech.
#[test]
fn observed_report_capexit_notice_identity_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = State::default();
    state.bind(dir.path(), &Scope::All);
    state.render(dir.path(), &Scope::All, "");
    let before = state.pinned.as_ref().unwrap().0;
    state.set_notices(
        dir.path(),
        &[format!(
            "Captured working state: {}\n## Model explanation\n```",
            "x".repeat(9000)
        )],
    );
    let report = state.render(dir.path(), &Scope::All, "");
    assert_ne!(state.pinned.as_ref().unwrap().0, before);
    assert!(report.contains("retention limit reached"));
    assert_eq!(
        assistant_prose(&format!("{report}Model words.")),
        "Model words."
    );
    state.set_notices(dir.path(), &["".into()]);
    let report = state.render(dir.path(), &Scope::All, "");
    assert_eq!(
        assistant_prose(&format!("{report}Model words.")),
        "Model words."
    );
}
