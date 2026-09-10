use super::*;

// --- PR4: the `git` tool arm in execute_tool ---------------------------

/// A stub GitTool: echoes the op, and refuses `commit` when the projected
/// GitCaveats deny it — exercises the arm's caveat projection without a repo.
struct StubGit;

impl crate::agentic::GitTool for StubGit {
    fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        caps: &crate::git_caveats::GitCaveats,
        _session: &Caveats,
    ) -> Result<String, String> {
        match op {
            "status" => Ok("on branch main (HEAD abc123)".to_string()),
            "branch-list" => Ok("local branches: 1\nrefs/heads/main".to_string()),
            "commit" if !caps.permits_commit() => {
                Err("capability denied: git commit not permitted".to_string())
            }
            "commit" => Ok("committed abc123: msg".to_string()),
            // #1191: data-loss ops the gate guards — if we reach here, the
            // gate ALLOWED (the refusal path returns before dispatch).
            "stash-drop" => Ok("dropped stash@{0}".to_string()),
            "branch-delete" => Ok("deleted branch feature".to_string()),
            other => Err(format!("unknown git op '{other}'")),
        }
    }
}

#[tokio::test]
async fn read_only_git_dispatch_accepts_reads_without_granting_writes() {
    // Share the routing-switch tests' lock and restore every override. This
    // fixture requires routing enabled with OCAP enforcement still on.
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _routing = super::disable_ocap_tests::EnvVar::set("NEWT_NO_ROUTE", "0");
    let _ocap = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let imperative = crate::agentic::PromptIntake::analyze(
        "please count the branches in this repo (newt-agent repo)",
    );
    assert_eq!(imperative.disposition(), PromptDisposition::Act);
    let question = crate::agentic::PromptIntake::analyze(
        "Can you tell me how many branches are open in this repo?",
    );
    assert_eq!(question.disposition(), PromptDisposition::Explain);
    let ws = tempfile::tempdir().unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        ..Caveats::top()
    };
    for disposition in [
        imperative.disposition(),
        question.disposition(),
        PromptDisposition::Research,
        PromptDisposition::Plan,
    ] {
        for (name, args, accepted) in [
            ("git", serde_json::json!({"op": "branch-list"}), true),
            ("git", serde_json::json!({"op": "status"}), false),
            (
                "git",
                serde_json::json!({"op": "branch", "name": "created"}),
                false,
            ),
            (
                "git",
                serde_json::json!({"op": "commit", "message": "unexpected"}),
                false,
            ),
            (
                "git",
                serde_json::json!({"op": "branch-delete", "name": "main"}),
                false,
            ),
            ("git", serde_json::json!({"op": "fetch"}), false),
            ("git", serde_json::json!({}), false),
            (
                "run_command",
                serde_json::json!({"command": "git branch --all"}),
                true,
            ),
            (
                "run_command",
                serde_json::json!({"command": "git branch --all", "cwd": "elsewhere"}),
                false,
            ),
            (
                "run_command",
                serde_json::json!({"command": "git branch created"}),
                false,
            ),
        ] {
            if name == "run_command" {
                let (presented_name, _) = tool_presentation(name, &args, ws.path());
                assert_eq!(
                    presented_name,
                    if accepted { "git" } else { "run_command" },
                    "presentation must not discard argument semantics: {args}"
                );
            }
            let mut gate = MockGate::new(true, &caveats);
            let out = execute_tool_with_collaborators(
                name,
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut NoMcp,
                ToolCollaborators {
                    git_tool: Some(&StubGit),
                    permission_gate: if disposition == PromptDisposition::Act {
                        None
                    } else {
                        Some(&mut gate)
                    },
                    ..Default::default()
                },
                false,
                disposition,
                None,
            )
            .await
            .expect("legacy fixture has no durable writer")
            .unwrap();
            assert_eq!(
                out.contains("local branches: 1") || out.contains("on branch main"),
                accepted,
                "{disposition:?} {name} {args}: {out}"
            );
            if !accepted && disposition != PromptDisposition::Act {
                assert!(
                    out.contains("not available for this request"),
                    "must reject before dispatch: {out}"
                );
            }
            assert!(
                gate.asks.is_empty(),
                "read-only calls never request an authority grant"
            );
        }
        let allow = vec!["read_file".to_owned()];
        let out = execute_tool_with_collaborators(
            "run_command",
            &serde_json::json!({"command": "git branch"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                git_tool: Some(&StubGit),
                persona_tools: Some(&allow),
                ..Default::default()
            },
            false,
            disposition,
            None,
        )
        .await
        .expect("legacy fixture has no durable writer")
        .unwrap();
        assert!(
            out.contains("persona") && !out.contains("local branches: 1"),
            "{out}"
        );
    }
}

async fn run_git(op: &str, caveats: &Caveats, git: Option<&dyn crate::agentic::GitTool>) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        "git",
        &serde_json::json!({ "op": op }),
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        git,
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

#[tokio::test]
async fn git_arm_dispatches_when_injected() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("status", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(out.contains("on branch main"), "got: {out}");
}

#[tokio::test]
async fn git_arm_surfaces_denials_from_projected_caveats() {
    // A session with no fs_write → from_session denies commit_local.
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_git("commit", &read_only, Some(&StubGit)).await;
    assert!(
        out.contains("error:") && out.contains("commit"),
        "got: {out}"
    );
    // The same session can still run a read op.
    let out = run_git("status", &read_only, Some(&StubGit)).await;
    assert!(out.contains("on branch main"), "got: {out}");
}

/// Same as `run_git` but with a gate AND the git tool both injected — the
/// #1056 path where a denied git write consults the operator.
#[allow(clippy::too_many_arguments)]
async fn run_git_gated(
    op: &str,
    caveats: &Caveats,
    git: &dyn crate::agentic::GitTool,
    gate: &mut MockGate,
) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        "git",
        &serde_json::json!({ "op": op }),
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        Some(gate), // permission_gate
        None,       // exec_floor
        Some(git),  // git_tool
        None,       // crew_runner
        None,       // scratchpad_store
        None,       // code_search
        None,       // where_is
        None,       // experience_store
        None,       // step_ledger
    )
    .await
}

#[tokio::test]
async fn git_data_loss_ops_are_gated_even_under_full_write_authority() {
    // #1191: the exact catastrophe — a confused model tries to destroy
    // work (stash-drop / branch-delete). Even with FULL write authority
    // (the --full-access analogue), the op is refused without an explicit
    // operator confirmation, and proceeds only WITH it. Safe ops never
    // consult the data-loss gate.
    let ws = tempfile::TempDir::new().unwrap();
    let full = caveats_rw(ws.path());

    // Gate DECLINES → refused, StubGit never dropped the stash.
    let mut deny = MockGate::new(false, &full);
    let out = run_git_gated("stash-drop", &full, &StubGit, &mut deny).await;
    assert!(
        out.starts_with("refused:"),
        "must refuse without confirm: {out}"
    );
    assert!(
        !out.contains("dropped stash"),
        "the drop must NOT have run: {out}"
    );
    assert!(
        deny.asks
            .iter()
            .any(|(t, k)| t == "git" && k.contains("stash-drop")),
        "the data-loss confirmation was asked: {:?}",
        deny.asks
    );

    // Gate ALLOWS → proceeds.
    let mut allow = MockGate::new(true, &full);
    let out = run_git_gated("branch-delete", &full, &StubGit, &mut allow).await;
    assert!(
        out.contains("deleted branch"),
        "confirmed → proceeds: {out}"
    );

    // A SAFE op is never gated as data-loss.
    let mut g = MockGate::new(false, &full);
    let out = run_git_gated("status", &full, &StubGit, &mut g).await;
    assert!(out.contains("on branch main"), "safe op runs: {out}");
    assert!(
        !g.asks
            .iter()
            .any(|(_, k)| k.contains("stash-drop") || k.contains("branch-delete")),
        "status must not trip the data-loss gate: {:?}",
        g.asks
    );
}

#[tokio::test]
async fn git_data_loss_op_refused_headless_no_gate() {
    // No permission gate (headless) → a data-loss op is refused, never run.
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("stash-drop", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(
        out.starts_with("refused:"),
        "headless data-loss refused: {out}"
    );
}

#[tokio::test]
async fn git_write_denial_routes_through_gate_and_commits_on_allow() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &read_only);
    let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("committed"),
        "gate-granted commit should land: {out}"
    );
    assert!(
        gate.asks
            .iter()
            .any(|(t, k)| t == "git" && k.starts_with("git_write:commit")),
        "a git_write grant was requested: {:?}",
        gate.asks
    );
}

/// Deny-by-default invariant: a gate that DECLINES keeps the git write denied.
#[tokio::test]
async fn git_write_denied_when_operator_declines() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &read_only);
    let out = run_git_gated("commit", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("capability denied: git commit not permitted"),
        "a declined git write stays denied: {out}"
    );
}

/// #1056: a git READ is never gated — the arm only routes WRITE denials, so a
/// read op never even consults the gate.
#[tokio::test]
async fn git_read_is_never_gated() {
    let ws = tempfile::TempDir::new().unwrap();
    let read_only = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &read_only);
    let out = run_git_gated("status", &read_only, &StubGit, &mut gate).await;
    assert!(
        out.contains("on branch main"),
        "read op runs ungated: {out}"
    );
    assert!(gate.asks.is_empty(), "a read must not consult the gate");
}

#[tokio::test]
async fn git_arm_unknown_op_is_an_error_not_a_panic() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("frobnicate", &caveats_rw(ws.path()), Some(&StubGit)).await;
    assert!(
        out.contains("error:") && out.contains("unknown git op"),
        "got: {out}"
    );
}

#[tokio::test]
async fn git_arm_without_injection_is_unknown_tool() {
    let ws = tempfile::TempDir::new().unwrap();
    let out = run_git("status", &caveats_rw(ws.path()), None).await;
    assert!(out.contains("unknown tool: git"), "got: {out}");
}
