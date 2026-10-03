use super::*;
use crate::agentic::ReadScope;
use crate::ExecOutcome;

/// Brush rejects this cwd before starting its worker, so the unit harness can
/// exercise the actual out-of-band ToolError even though normal unit dispatch
/// substitutes the safe-subset engine for Brush's worker re-exec.
#[tokio::test]
async fn brush_cwd_error_preserves_denied_execution_class() {
    let parent = tempfile::tempdir().unwrap();
    let parent = parent.path().canonicalize().unwrap();
    let workspace = parent.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let registry = agent_bridle::Registry::builder()
        .tool(std::sync::Arc::new(agent_bridle::BrushShellTool::new()))
        .build();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let grant = registry.mint_grant(caveats);
    let error = registry
        .dispatch(
            "shell",
            serde_json::json!({"cmd": "printf forbidden > marker", "cwd": parent}),
            &grant,
        )
        .await
        .expect_err("Brush must refuse the parent before worker creation");
    assert!(matches!(error, agent_bridle::ToolError::Denied { .. }));
    let (text, outcome) = shell::dispatch_error_result(error);
    assert_eq!(outcome, ExecOutcome::Denied, "{text}");
    assert!(!parent.join("marker").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn denied_cwd_reports_workspace_and_retains_empty_write_scope() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let parent = tempfile::tempdir().unwrap();
    let parent = parent.path().canonicalize().unwrap();
    let workspace = parent.join("child \"quoted\"\nline");
    std::fs::create_dir(&workspace).unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::none(),
        ..Caveats::top()
    };
    let before = serde_json::to_value(&caveats).unwrap();
    let execution = std::sync::OnceLock::new();
    let result = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "printf CWD_MUST_NOT_RUN", "cwd": parent}),
        &workspace.to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        ToolCollaborators {
            execution: Some(&execution),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(execution.get(), Some(&ExecOutcome::Denied), "{result}");
    assert!(
        result.contains(&format!("Workspace root: {}", serde_json::json!(workspace))),
        "{result}"
    );
    assert!(
        result.contains(&format!(
            "Requested command directory: {}",
            serde_json::json!(parent)
        )),
        "{result}"
    );
    assert!(
        result.contains(&format!("fs_read={}", serde_json::json!([workspace]))),
        "{result}"
    );
    assert!(result.contains("fs_write=[]"), "{result}");
    assert!(!result.contains("CWD_MUST_NOT_RUN"), "{result}");
    assert_eq!(serde_json::to_value(&caveats).unwrap(), before);
}

#[test]
fn denial_context_quotes_paths_and_bounds_scope_details() {
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only((0..6).map(|i| format!("/scope/{i}\"\n"))),
        ..Caveats::top()
    };
    let text = denial_context("/workspace/\"\n", Some("/requested/\"\n"), Some(&caveats));
    assert!(
        text.contains(r#"Workspace root: "/workspace/\"\n""#),
        "{text}"
    );
    assert!(
        text.contains(r#"Requested command directory: "/requested/\"\n""#),
        "{text}"
    );
    assert!(text.contains("fs_read=all"), "{text}");
    assert!(text.contains("(2 more roots)"), "{text}");
    assert_eq!(
        text.lines().count(),
        4,
        "paths must not inject context lines"
    );
}

#[test]
fn declined_permissions_report_defaults_without_refreshing_authority() {
    struct NoDiagnosticRefresh;
    impl PermissionGate for NoDiagnosticRefresh {
        fn refresh_caveats(&mut self, _: &Caveats) -> PermissionDecision {
            panic!("permission diagnostics must not refresh or remint authority")
        }
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            assert_eq!(requests.len(), 1);
            PermissionDecision::Deny
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }
    for capability in ["fs_read", "fs_write"] {
        let target = "/parent/\"\n";
        let args = serde_json::json!({"capability": capability, "target": target});
        for has_gate in [false, true] {
            let mut gate = NoDiagnosticRefresh;
            let out = execute_request_permissions(
                &args,
                has_gate.then_some(&mut gate as &mut dyn PermissionGate),
                false,
                20,
                "/parent/child",
                None,
            )
            .2;
            // The tool trims requested targets; diagnostics must reflect the
            // actual target asked of the gate and escape it as data.
            assert!(
                out.contains(&serde_json::json!(target.trim()).to_string()),
                "{out}"
            );
            assert!(out.contains("Workspace root: \"/parent/child\""), "{out}");
            assert!(!out.contains("fs_read="), "{out}");
            assert!(!out.contains("fs_write="), "{out}");
        }
    }
}

#[test]
fn denial_like_child_text_does_not_become_host_authority_evidence() {
    let stderr = "denied: read of /somewhere is not within the granted fs_read scope";
    let envelope = serde_json::json!({"exit_code": 1, "stdout": "", "stderr": stderr});
    let (text, outcome) =
        shell::confined_result("check", &envelope, &Caveats::top(), false, |_| {
            stderr.into()
        });
    assert_eq!(outcome, ExecOutcome::Failed);
    assert_eq!(text, stderr);
    let (text, outcome) =
        shell::dispatch_error_result(agent_bridle::ToolError::Other(anyhow::anyhow!(stderr)));
    assert_eq!(outcome, ExecOutcome::Unavailable);
    assert_eq!(text, format!("error: {stderr}"));
}

// -- #721 recoverable denials + request_permissions ---------------------

#[test]
fn exec_denial_is_recoverable_not_a_dead_end() {
    // #721 + #775: the exec denial the MODEL sees is ONE clean level —
    // `capability denied: <bare reason>. <recovery hint>` — leading to the
    // model-actionable request_permissions path, NOT the stale `extra_exec`
    // config edit (which #721 superseded and the model cannot perform
    // mid-turn).
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "mkdir",
            "reason": "exec of \"mkdir\" is not within the granted authority"
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    assert!(out.starts_with("capability denied:"), "got: {out}");
    assert!(out.contains("request_permissions"), "got: {out}");
    // #775: the stale `extra_exec` config hint is GONE from the model-facing
    // message (it leaked in before).
    assert!(
        !out.contains("extra_exec"),
        "the model message must not carry the stale config hint: {out}"
    );
}

/// #775 (§2.5) regression: the model-facing `run_command` denial is ONE
/// clean level and never a denial sentence NESTED inside another. Before
/// the fix, `denied_run_command_result` appended the `extra_exec` config
/// hint to the reason (and the former notice stuffed that whole sentence into
/// its bare `'{target}'` slot), yielding `capability denied: exec does not
/// permit '<reason> - add it via …>'`. The model-facing return now carries
/// exactly one `capability denied:`, the bare reason, and the recovery hint.
#[test]
fn run_command_denial_is_single_level_not_nested() {
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "export",
            "reason": "exec of \"export\" is not within the granted authority"
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    // Exactly one denial prefix — never a `capability denied:` inside another.
    assert_eq!(
        out.matches("capability denied:").count(),
        1,
        "exactly one denial level: {out}"
    );
    // RED on today: the stale config hint was glued onto the model message.
    assert!(!out.contains("add it via"), "stale config hint: {out}");
    assert!(!out.contains("extra_exec"), "stale config hint: {out}");
    // No reason sentence nested inside a `does not permit '…'` slot.
    assert!(
        !out.contains("does not permit 'exec of"),
        "nested denial sentence: {out}"
    );
    // The bare reason and the #721 recovery hint are both present.
    assert!(
        out.contains("exec of \"export\" is not within the granted authority"),
        "got: {out}"
    );
    assert!(out.contains("request_permissions"), "got: {out}");
}

#[test]
fn run_command_denial_hints_preserve_each_axis_and_target() {
    let denials = [
        ("exec", "/usr/bin/helper", "/usr/bin/helper"),
        ("fs_read", "/", "/"),
        (
            "fs_write",
            "/workspace/report \"draft\".md",
            "/workspace/report \"draft\".md",
        ),
        ("net", "api.example.test", "api.example.test"),
    ];
    let envelope = serde_json::json!({
        "denials": denials.iter().map(|(kind, target, _)| {
            serde_json::json!({"kind": kind, "target": target, "reason": "outside granted authority"})
        }).collect::<Vec<_>>()
    });
    let out = denied_run_command_result(&envelope, false);
    assert_eq!(out.matches("capability denied:").count(), 1, "{out}");
    assert_eq!(
        out.matches("request_permissions(").count(),
        denials.len(),
        "{out}"
    );
    for (kind, _, granted_target) in denials {
        let expected = format!(
            "request_permissions(capability={kind:?}, target={}",
            serde_json::json!(granted_target)
        );
        assert!(out.contains(&expected), "missing {expected}: {out}");
    }
}

#[test]
fn run_command_ungrantable_denials_do_not_invent_permission_requests() {
    for denials in [
        serde_json::json!([]),
        serde_json::json!([{"kind": "open", "target": "/workspace/data"}]),
        serde_json::json!([{"kind": "fs_read"}]),
        serde_json::json!([{"target": "/workspace/data"}]),
        serde_json::json!([{"kind": "exec", "target": ""}]),
        serde_json::json!([{"kind": "exec", "target": "   "}]),
        serde_json::json!([{
            "kind": "exec", "target": "dynamic syntax",
            "reason": "refused by design: dynamic construct the confined shell does not interpret"
        }]),
        serde_json::json!([
            {"kind": "exec", "target": "helper"},
            {"kind": "exec", "target": "unsupported syntax",
             "reason": "not yet supported by the confined shell engine"}
        ]),
        serde_json::json!([
            {"kind": "exec", "target": "helper"},
            {"kind": "open", "target": "/workspace/data"}
        ]),
    ] {
        let out = denied_run_command_result(&serde_json::json!({"denials": denials}), false);
        assert!(!out.contains("request_permissions"), "{out}");
        assert!(out.contains("different approach"), "{out}");
    }
    let out = denied_run_command_result(&serde_json::json!({}), false);
    assert!(!out.contains("request_permissions"), "{out}");
}

/// One structured leash denial, in the envelope shape the interceptor records
/// (`denied: true` + one `denials[]` row).
fn one_denial(kind: &str, target: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "denied": true,
        "denials": [{"kind": kind, "target": target, "reason": reason}],
    })
}

/// The #2629 contract for a grantable denial: exactly one `request_permissions`
/// call, naming this axis and this exact target, and no "take a different
/// approach" fallback beside it.
fn assert_one_precise_request(out: &str, axis: &str, target: &str) {
    assert_eq!(out.matches("request_permissions(").count(), 1, "{out}");
    assert!(
        out.contains(&format!(
            r#"request_permissions(capability="{axis}", target={}"#,
            serde_json::json!(target)
        )),
        "{out}"
    );
    assert!(!out.contains("No actionable capability grant"), "{out}");
}

/// The leash's recorded reason for a refused `open`: `ToolError`'s Display
/// prefix + the `check_path` message (vendored `context.rs`; a live capture is
/// in docs/findings/2026-08-exec-mcp-interrupt-audit.md). `op` is the verb the
/// leash writes — `read` or `write` — which is the only place the axis lives.
fn open_reason(op: &str, target: &str) -> String {
    format!("denied: {op} of {target} (resolved {target}) is not within the granted fs_{op} scope")
}

/// A platform-valid ABSOLUTE path for a hand-built denial-envelope fixture.
/// These fixtures are pure text — the envelope never touches a real
/// filesystem — so a literal drive-qualified path is as good as a real one;
/// what matters is that `Path::is_absolute` agrees with the fixture's intent.
/// `/outside/…` is NOT absolute on Windows (no drive prefix, just a root),
/// so the production anchoring guard correctly withholds the hint there and
/// a Unix-only literal fails native Windows CI (#2629 round 3 review item 2).
/// Forward slashes avoid a JSON/backslash-escaping mismatch between this and
/// the production renderer's own `serde_json::json!` escaping.
#[cfg(windows)]
fn outside_path(relative: &str) -> String {
    format!("C:/outside/{relative}")
}
#[cfg(not(windows))]
fn outside_path(relative: &str) -> String {
    format!("/outside/{relative}")
}

/// #2629: agent-bridle's structured denial kinds are `exec` / `open` / `net`
/// (vendored `envelope.rs`), so a refused shell write — the `echo x > path`
/// probe a model uses to discover what is writable — arrives as kind `open`
/// and used to be dropped as "No actionable capability grant is identified".
/// That is what sent the lab model probing with touch/mkdir. The LEASH's own
/// reason names the axis (`write of …`; bridle carries no typed axis, so this
/// is the leading verb of a library-generated string) and brush hands the
/// interceptor the ABSOLUTE path, so the denial can name axis, exact target
/// and the one `request_permissions` call. Nothing here reads child output:
/// `reason` is written by the leash (#2633). RED on origin/main 71719b35: no
/// `request_permissions` at all for an `open` kind. The five structured kinds
/// are each measured in a test of their own (this one and the four below), so
/// one failing case cannot hide another.
#[test]
fn a_shell_open_write_denial_names_fs_write_and_the_exact_target() {
    let target = outside_path("wt/probe");
    let out = denied_run_command_result(
        &one_denial("open", &target, &open_reason("write", &target)),
        false,
    );
    assert_one_precise_request(&out, "fs_write", &target);
}

/// The read twin (`source`, `< path`), measured on its own. RED on origin/main
/// 71719b35 for the same reason as the write case.
#[test]
fn a_shell_open_read_denial_names_fs_read_and_the_exact_target() {
    let target = outside_path("wt/notes");
    let out = denied_run_command_result(
        &one_denial("open", &target, &open_reason("read", &target)),
        false,
    );
    assert_one_precise_request(&out, "fs_read", &target);
}

/// `exec` named its axis and exact target before #2629 (`denial_recovery_hints`
/// always took the kind as the axis); pinned per kind so it is measured
/// independently of `open`. Green on origin/main by construction.
#[test]
fn a_shell_exec_denial_names_exec_and_the_exact_target() {
    let out = denied_run_command_result(
        &one_denial(
            "exec",
            "/usr/bin/git",
            "denied: exec of \"/usr/bin/git\" is not within the granted authority",
        ),
        false,
    );
    assert_one_precise_request(&out, "exec", "/usr/bin/git");
}

/// `net` likewise: the target is the CONNECT host the egress proxy refused
/// (#196), and the one call names it. Green on origin/main by construction.
#[test]
fn a_shell_net_denial_names_net_and_the_exact_host() {
    let out = denied_run_command_result(
        &one_denial(
            "net",
            "github.com",
            "denied: network access to \"github.com\" is not within the granted authority",
        ),
        false,
    );
    assert_one_precise_request(&out, "net", "github.com");
}

/// A write whose parent directory does not exist yet is refused with the
/// leash's `cannot canonicalize` reason, which still names the operation
/// (`write of …`). The axis comes from that verb, so a first write into a new
/// directory is as grantable as one into an existing directory.
#[test]
fn an_open_denial_without_a_resolved_path_still_names_its_axis() {
    let target = outside_path("new-dir/wt");
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "open",
            "target": &target,
            "reason": format!(
                "denied: write of {target:?} denied: cannot canonicalize (No such file or directory (os error 2))"
            ),
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    assert!(
        out.contains(&format!(
            r#"request_permissions(capability="fs_write", target={}"#,
            serde_json::json!(target)
        )),
        "{out}"
    );
}

/// The axis is never guessed. The verb the leash writes BEFORE the model's
/// path decides it; a path that spells the other axis cannot steer it, and a
/// suffix that contradicts the verb yields no hint. A target the grant could
/// not cover — relative (the safe-subset engine records the redirect literal
/// as typed) or a glob pattern — falls back to the no-grant text rather than
/// suggesting a grant that would be inserted verbatim and never match.
#[test]
fn an_open_denial_hint_is_anchored_and_absolute_only() {
    let steer = outside_path("fs_write_notes");
    let envelope = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "open",
            "target": &steer,
            "reason": format!(
                "denied: read of {steer} (resolved {steer}) is not within the granted fs_read scope"
            ),
        }]
    });
    let out = denied_run_command_result(&envelope, false);
    assert!(out.contains(r#"capability="fs_read""#), "{out}");
    assert!(!out.contains(r#"capability="fs_write""#), "{out}");
    for (target, reason) in [
        (
            "wt/.probe",
            "denied: write of wt/.probe (resolved /ws/wt/.probe) is not within the granted fs_write scope",
        ),
        (
            "/outside/*.log",
            "denied: read of /outside (resolved /outside) is not within the granted fs_read scope",
        ),
        (
            "/outside/x",
            "denied: write of /outside/x (resolved /outside/x) is not within the granted fs_read scope",
        ),
        ("/outside/x", "denied: run cancelled (timeout or interrupt)"),
    ] {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [{"kind": "open", "target": target, "reason": reason}]
        });
        let out = denied_run_command_result(&envelope, false);
        assert!(!out.contains("request_permissions"), "{target}: {out}");
    }
}

/// Replay of the v0.8.0 gate's first step (#2631, #2629): `git worktree add`
/// outside the workspace. The two halves the harness CAN attribute each yield
/// ONE precise request, never a probing loop: the shell-redirect write probe
/// (the only write the brush interceptor itself sees; RED before the fix, no
/// grant was offered) and `git` itself outside the exec grant (already one
/// request; pinned so the pair stays symmetric). git's own helper exec and a
/// kernel-fenced coreutils write carry no structured evidence (#2421) and are
/// deliberately NOT represented here — see shell.rs `confined_result`.
#[test]
fn a_denied_worktree_write_probe_yields_one_precise_request_not_a_loop() {
    let probe = outside_path("newt-wt/.probe");
    let write = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "open",
            "target": &probe,
            "reason": format!(
                "denied: write of {probe} (resolved {probe}) is not within the granted fs_write scope"
            ),
        }]
    });
    let out = denied_run_command_result(&write, false);
    assert_eq!(out.matches("request_permissions(").count(), 1, "{out}");
    assert!(
        out.contains(&format!(
            r#"capability="fs_write", target={}"#,
            serde_json::json!(probe)
        )),
        "{out}"
    );
    let exec = serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "/usr/bin/git",
            "reason": "denied: exec of \"/usr/bin/git\" is not within the granted authority",
        }]
    });
    let out = denied_run_command_result(&exec, false);
    assert_eq!(out.matches("request_permissions(").count(), 1, "{out}");
    assert!(
        out.contains(r#"capability="exec", target="/usr/bin/git""#),
        "{out}"
    );
}

/// Caveats under which every native fs tool is denied, in a workspace at `ws`.
fn no_fs_caveats(ws: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::none(),
        fs_write: Scope::none(),
        ..caveats_rw(ws)
    }
}

/// #2629: a native fs tool's denial suggests the EXACT path the #263 gate
/// would be asked for — the workspace-joined absolute path — not the model's
/// relative spelling. `caveats::permits_path` is a lexical prefix test, so a
/// relative root granted via `request_permissions` never covers the absolute
/// retry: the model would ask, be told "granted", retry and be denied again.
/// RED on origin/main 71719b35: the hint said `target="a.txt"`.
#[tokio::test]
async fn a_native_write_denial_suggests_the_absolute_target_the_gate_would_grant() {
    let ws = tempfile::TempDir::new().unwrap();
    let full = ws.path().join("a.txt").to_string_lossy().into_owned();
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "c"}),
        ws.path(),
        &no_fs_caveats(ws.path()),
        None,
    )
    .await;
    assert_one_precise_request(&out, "fs_write", &full);
}

/// The read twin, measured on its own. RED on origin/main 71719b35 for the
/// same reason as the write case.
#[tokio::test]
async fn a_native_read_denial_suggests_the_absolute_target_the_gate_would_grant() {
    let ws = tempfile::TempDir::new().unwrap();
    let full = ws.path().join("a.txt").to_string_lossy().into_owned();
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "a.txt"}),
        ws.path(),
        &no_fs_caveats(ws.path()),
        None,
    )
    .await;
    assert_one_precise_request(&out, "fs_read", &full);
}

#[test]
fn parse_capability_maps_synonyms_and_rejects_unknown() {
    assert_eq!(parse_capability("exec"), Some(DenialKind::Exec));
    assert_eq!(parse_capability("shell"), Some(DenialKind::Exec));
    assert_eq!(parse_capability("FS_READ"), Some(DenialKind::FsRead));
    assert_eq!(parse_capability("write"), Some(DenialKind::FsWrite));
    assert_eq!(parse_capability("network"), Some(DenialKind::Net));
    assert_eq!(parse_capability("gpu"), None);
    assert_eq!(parse_capability(""), None);
}

#[test]
fn request_permissions_grant_deny_and_no_gate() {
    let base = Caveats::top();

    // Mock gate ALLOWS → "granted" + the retry coaching; the gate was asked
    // with the parsed axis + target.
    let mut gate = MockGate::new(true, &base);
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "make a dir"}),
        Some(&mut gate),
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    assert!(out.starts_with("granted:"), "got: {out}");
    assert!(out.contains("Retry the original operation"), "got: {out}");
    assert_eq!(gate.asks.len(), 1);
    assert_eq!(
        gate.asks[0],
        ("request_permissions".to_string(), "exec:mkdir".to_string())
    );

    // Mock gate DENIES → "denied" + don't-retry coaching.
    let mut gate = MockGate::new(false, &base);
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "fs_write", "target": "/tmp/x", "reason": "w"}),
        Some(&mut gate),
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    assert!(out.starts_with("denied:"), "got: {out}");
    assert!(out.contains("different approach"), "got: {out}");

    // NO gate (headless / eval) → "no operator available" — recoverable,
    // never a hang or a config-only dead end.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "net", "target": "docs.rs", "reason": "fetch"}),
        None,
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    assert!(out.contains("no operator available"), "got: {out}");
}

#[test]
fn permission_grant_releases_only_cached_authority_failures() {
    use crate::agentic::RepeatCallGuard;

    let args = serde_json::json!({"path": "report.txt"});
    let permission = serde_json::json!({"capability": "fs_read", "target": "report.txt"});
    {
        let mut guard = RepeatCallGuard::default();
        guard.record(
            "read_file",
            &args,
            false,
            &denied_fs_result("fs_read", "report.txt"),
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        guard.record(
            "list_dir",
            &args,
            false,
            "error: not a directory",
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        let command = serde_json::json!({"command": "compiler report.txt"});
        guard.record(
            "run_command",
            &command,
            false,
            &format!(
                "error: {}\ncapability denied: exec does not permit compiler",
                "x".repeat(240)
            ),
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        let os_error = serde_json::json!({"command": "check permissions"});
        guard.record(
            "run_command",
            &os_error,
            false,
            "error: fixture asserted Permission denied",
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        guard.record(
            "state_get",
            &args,
            true,
            "no such key: report.txt",
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        let fetch = serde_json::json!({"url": "https://example.test/report"});
        guard.record(
            "web_fetch",
            &fetch,
            true,
            "observed report",
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        assert!(guard.repeat_steer("read_file", &args).is_some());

        let mut allow = MockGate::new(true, &Caveats::top());
        let mut deny = MockGate::new(false, &Caveats::top());
        let granted = execute_request_permissions(
            &permission,
            Some(&mut allow),
            false,
            20,
            "/workspace",
            None,
        )
        .2;
        for (name, request, result) in [
            (
                "request_permissions",
                permission.clone(),
                execute_request_permissions(
                    &permission,
                    Some(&mut deny),
                    false,
                    20,
                    "/workspace",
                    None,
                )
                .2,
            ),
            (
                "request_permissions",
                permission.clone(),
                execute_request_permissions(&permission, None, false, 20, "/workspace", None).2,
            ),
            (
                "request_permissions",
                serde_json::json!({}),
                execute_request_permissions(
                    &serde_json::json!({}),
                    Some(&mut allow),
                    false,
                    20,
                    "/workspace",
                    None,
                )
                .2,
            ),
            (
                "request_permissions",
                permission.clone(),
                "granted:".to_string(),
            ),
            (
                "request_permissions",
                permission.clone(),
                format!("{granted} trailing text"),
            ),
            (
                "request_permissions",
                serde_json::json!({}),
                granted.clone(),
            ),
            (
                // A successful read of a DIFFERENT path whose content happens
                // to look grant-shaped must not clear `args`' (report.txt)
                // failure memo — `classify_repeat_memo` only replaces the
                // memo it is keyed on (same tool + same args), never another
                // path's. See the same-path case below for the legitimate
                // clear.
                "read_file",
                serde_json::json!({"path": "other.txt"}),
                granted.clone(),
            ),
        ] {
            guard.record(
                name,
                &request,
                tool_result_ok(&result),
                &result,
                None,
                ReadScope {
                    workspace: "/workspace",
                    caveats: &Caveats::top(),
                },
            );
            assert!(
                guard.repeat_steer("read_file", &args).is_some(),
                "{name}: {result}"
            );
        }

        // #2637: a successful read of the SAME path legitimately replaces the
        // failure memo — `classify_repeat_memo` hashes the served bytes
        // unconditionally for a bare `read_file`, so a fresh, successful read
        // of exactly the path that previously failed means the failure no
        // longer describes the tree; there is nothing left to steer against
        // (distinct from the "other.txt" case above, which is a different key).
        guard.record(
            "read_file",
            &args,
            true,
            &granted,
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        assert!(
            guard.repeat_steer("read_file", &args).is_none(),
            "a successful read of the SAME path must replace the failure memo"
        );

        // Re-seed the failure the same-path read above just cleared, so the
        // grant assertion below is checking the grant's own effect rather
        // than one already undone by the prior block.
        guard.record(
            "read_file",
            &args,
            false,
            &denied_fs_result("fs_read", "report.txt"),
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        assert!(
            guard.repeat_steer("read_file", &args).is_some(),
            "re-seeded failure must be present before the grant clears it"
        );

        guard.record(
            "request_permissions",
            &permission,
            tool_result_ok(&granted),
            &granted,
            None,
            ReadScope {
                workspace: "/workspace",
                caveats: &Caveats::top(),
            },
        );
        assert!(
            guard.repeat_steer("read_file", &args).is_none(),
            "a confirmed grant must allow re-evaluation"
        );
        assert!(
            guard.repeat_steer("run_command", &command).is_none(),
            "wrapped denials survive first-line truncation"
        );
        assert!(
            guard.repeat_steer("run_command", &os_error).is_some(),
            "arbitrary OS-error text is not a capability denial"
        );
        assert!(
            guard.repeat_steer("list_dir", &args).is_some(),
            "ordinary I/O failures stay memoized"
        );
        assert!(guard.repeat_steer("state_get", &args).is_some());
        assert!(guard.repeat_steer("web_fetch", &fetch).is_some());
        assert_eq!(
            guard.total_failures(),
            5,
            "invalidation does not erase executed history (includes the re-seeded failure above)"
        );
    }
}

#[test]
fn permission_grant_releases_native_command_failure_for_recheck() {
    use crate::agentic::RepeatCallGuard;

    let command = serde_json::json!({"command": "/usr/bin/head -n 1 /approved/config"});
    let permission = serde_json::json!({"capability": "fs_read", "target": "/approved/config"});
    let unrelated = serde_json::json!({"path": "/other/report"});
    // This is the real confined-child result shape grounded by the native
    // session-grant test below; it carries no structured capability refusal.
    {
        for (name, outcome, released) in [
            ("run_command", Some(ExecOutcome::Failed), true),
            ("lifecycle", Some(ExecOutcome::Failed), true),
            ("run_command", Some(ExecOutcome::Denied), true),
            ("run_command", Some(ExecOutcome::TimedOut), false),
            ("run_command", Some(ExecOutcome::Unavailable), false),
            ("run_command", Some(ExecOutcome::Passed), false),
            ("run_command", None, false),
            ("read_file", Some(ExecOutcome::Failed), false),
        ] {
            for failed in [
                "error: command exited 1\nhead: /approved/config: Operation not permitted",
                "error: command exited 101\ncompilation failed",
            ] {
                let mut guard = RepeatCallGuard::default();
                guard.record(
                    name,
                    &command,
                    tool_result_ok(failed),
                    failed,
                    outcome,
                    ReadScope {
                        workspace: "/workspace",
                        caveats: &Caveats::top(),
                    },
                );
                guard.record(
                    "read_file",
                    &unrelated,
                    false,
                    "error: not a directory",
                    None,
                    ReadScope {
                        workspace: "/workspace",
                        caveats: &Caveats::top(),
                    },
                );
                assert!(guard.repeat_steer(name, &command).is_some());
                let declined =
                    execute_request_permissions(&permission, None, false, 20, "/workspace", None).2;
                guard.record(
                    "request_permissions",
                    &permission,
                    tool_result_ok(&declined),
                    &declined,
                    None,
                    ReadScope {
                        workspace: "/workspace",
                        caveats: &Caveats::top(),
                    },
                );
                assert!(guard.repeat_steer(name, &command).is_some());

                let mut gate = MockGate::new(true, &Caveats::top());
                let granted = execute_request_permissions(
                    &permission,
                    Some(&mut gate),
                    false,
                    20,
                    "/workspace",
                    None,
                )
                .2;
                guard.record(
                    "request_permissions",
                    &permission,
                    tool_result_ok(&granted),
                    &granted,
                    None,
                    ReadScope {
                        workspace: "/workspace",
                        caveats: &Caveats::top(),
                    },
                );
                assert_eq!(
                    guard.repeat_steer(name, &command).is_none(),
                    released,
                    "only a typed failed native call can re-enter confinement: {name} {outcome:?}"
                );
                assert!(guard.repeat_steer("read_file", &unrelated).is_some());
                assert_eq!(
                    guard.total_failures(),
                    2,
                    "executed failures remain history"
                );
                guard.record(
                    name,
                    &command,
                    tool_result_ok(failed),
                    failed,
                    outcome,
                    ReadScope {
                        workspace: "/workspace",
                        caveats: &Caveats::top(),
                    },
                );
                assert!(
                    guard.repeat_steer(name, &command).is_some(),
                    "a retry that still fails must be memoized again"
                );
            }
        }
    }
}

/// Grounds the cached-denial unit test in real dispatch and a regular file:
/// after the operator grants its exact path, the identical list_dir call must
/// reach the filesystem and report ENOTDIR, rather than replay a stale denial.
#[tokio::test]
async fn permission_grant_retry_reaches_the_real_file_error() {
    use crate::agentic::RepeatCallGuard;

    let workspace = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let file = outside.path().join("report.txt");
    std::fs::write(&file, "a regular file, not a directory").unwrap();
    let args = serde_json::json!({"path": file});
    let base = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(workspace.path())
    };
    let mut guard = RepeatCallGuard::default();
    let denied = run_tool("list_dir", args.clone(), workspace.path(), &base, None).await;
    assert!(denied.starts_with("capability denied:"), "{denied}");
    guard.record(
        "list_dir",
        &args,
        tool_result_ok(&denied),
        &denied,
        None,
        ReadScope {
            workspace: &workspace.path().to_string_lossy(),
            caveats: &base,
        },
    );
    assert!(guard.repeat_steer("list_dir", &args).is_some());

    let mut gate = MockGate::new(true, &base);
    let permission = serde_json::json!({"capability": "fs_read", "target": file});
    let granted =
        execute_request_permissions(&permission, Some(&mut gate), false, 20, "/workspace", None).2;
    assert!(granted.starts_with("granted:"), "{granted}");
    guard.record(
        "request_permissions",
        &permission,
        tool_result_ok(&granted),
        &granted,
        None,
        ReadScope {
            workspace: &workspace.path().to_string_lossy(),
            caveats: &base,
        },
    );
    assert!(
        guard.repeat_steer("list_dir", &args).is_none(),
        "the authorized retry must execute"
    );

    let retried =
        run_tool_gated("list_dir", args.clone(), workspace.path(), &base, &mut gate).await;
    let actual_error = std::fs::read_dir(&file).unwrap_err();
    assert_eq!(actual_error.kind(), std::io::ErrorKind::NotADirectory);
    assert_eq!(retried, format!("error: {actual_error}"));
    assert_eq!(gate.asks.len(), 2, "the retry still checks authority");
    assert!(gate
        .asks
        .iter()
        .all(|(_, target)| target == &format!("fs_read:{}", file.display())));
    guard.record(
        "list_dir",
        &args,
        tool_result_ok(&retried),
        &retried,
        None,
        ReadScope {
            workspace: &workspace.path().to_string_lossy(),
            caveats: &base,
        },
    );
    guard.record(
        "request_permissions",
        &permission,
        tool_result_ok(&granted),
        &granted,
        None,
        ReadScope {
            workspace: &workspace.path().to_string_lossy(),
            caveats: &base,
        },
    );
    assert!(
        guard.repeat_steer("list_dir", &args).is_some(),
        "a new grant cannot repair ENOTDIR"
    );
}

/// #1547: the headless `request_permissions` answer must be ACTIONABLE, not
/// a dead-end. With no gate, authority cannot be widened mid-run, so the
/// model must be told to (a) stop re-asking and (b) proceed within the
/// authority it already holds — NOT that "the owner must configure it"
/// (there is no owner mid-run) or to "take a different approach for now"
/// (which abandons a task the confined bench lane already authorizes and
/// burns tool-call rounds). Would fail on the old dead-end copy.
#[test]
fn request_permissions_headless_answer_is_forward_guidance_not_a_dead_end() {
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "fs_write", "target": "/app/out", "reason": "write result"}),
        None,
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    // Preserves the recoverable "no operator" signal.
    assert!(out.contains("no operator available"), "got: {out}");
    // Tells the model to proceed within its existing authority (forward
    // guidance) and that re-calling the tool is pointless headless.
    assert!(
        out.contains("Proceed within the authority you already have"),
        "headless answer must tell the model to proceed within current authority: {out}"
    );
    assert!(
        out.contains("re-calling request_permissions will not help"),
        "headless answer must tell the model not to keep asking: {out}"
    );
    // Must NOT re-route the model to a config edit it cannot perform
    // mid-run, or tell it to abandon its approach — the old dead-ends.
    assert!(
        !out.contains("must be configured by the owner"),
        "headless answer must not dead-end on an owner config edit: {out}"
    );
    assert!(
        !out.contains("take a different approach for now"),
        "headless answer must not tell the model to abandon its approach: {out}"
    );
}

#[test]
fn request_permissions_coaches_bad_inputs() {
    // Unknown capability → coach listing the valid axes (no gate consulted).
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "gpu", "target": "x", "reason": "y"}),
        None,
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    assert!(out.contains("unknown capability"), "got: {out}");
    assert!(out.contains("fs_read"), "got: {out}");
    // Missing target → coach.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "exec", "reason": "y"}),
        None,
        false,
        20,
        "/workspace",
        None,
    )
    .2;
    assert!(out.contains("'target' is required"), "got: {out}");
}

#[test]
fn request_permissions_is_a_real_tool_not_a_phantom() {
    // #721: a real, always-advertised tool — never an alias / hallucination.
    assert!(resolve_tool_alias("request_permissions").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"request_permissions"));
    assert!(classify_phantom_reach(
        "request_permissions",
        &serde_json::json!({"capability": "exec", "target": "mkdir", "reason": "r"}),
        "granted: the operator allowed exec for 'mkdir'.",
        true,
    )
    .is_none());
}

/// FLAG OFF (no gate): the denial is deterministic and still DENIES every
/// fs op (the #263 default-deny posture is intact) — now in the #721
/// recoverable form (`denied_fs_result`, carrying the request_permissions
/// path), pinned via the shared helper so the wording can't drift.
#[tokio::test]
async fn no_gate_denials_are_bit_for_bit_unchanged() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let out = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_read", &ws.path().join("secret.txt").to_string_lossy())
    );
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_read", &ws.path().join(".").to_string_lossy())
    );
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "c"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_write", &ws.path().join("a.txt").to_string_lossy())
    );
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_write", &ws.path().join("a.txt").to_string_lossy())
    );
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_write", &ws.path().join("secret.txt").to_string_lossy())
    );
    // #721: every fs denial now carries the model-actionable recovery path.
    assert!(out.contains("request_permissions"), "got: {out}");
}

/// Gate allows an fs_read denial → the read proceeds and returns the
/// real contents; the gate was consulted with the tool + axis + full
/// path it would be granting.
#[tokio::test]
async fn gate_allow_turns_fs_read_denial_into_the_real_result() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "the contents").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert_eq!(out, "the contents");
    let full = ws.path().join("secret.txt").to_string_lossy().into_owned();
    assert_eq!(
        gate.asks,
        vec![("read_file".to_string(), format!("fs_read:{full}"))]
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn permission_retry_closes_each_live_generation_before_the_next_starts() {
    let _l = super::disable_ocap_tests::env_lock().await;
    // Pin the engine for deterministic permission-retry behavior when the
    // workspace suite runs tests concurrently with ambient shell settings.
    let _eng = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    #[derive(Default)]
    struct LifecycleOutput(std::sync::Mutex<Vec<String>>);
    impl crate::agentic::LiveToolOutput for LifecycleOutput {
        fn start(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("start:{generation}"));
        }
        fn write(&self, generation: u64, _stream: crate::agentic::ToolOutputStream, chunk: &[u8]) {
            self.0.lock().unwrap().push(format!(
                "write:{generation}:{}",
                String::from_utf8_lossy(chunk)
            ));
        }
        fn finish(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("finish:{generation}"));
        }
        fn abandon(&self, generation: u64) {
            self.0.lock().unwrap().push(format!("abandon:{generation}"));
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let denied = Caveats {
        exec: Scope::none(),
        // Isolate exec retry lifecycle from macOS's unsupported network floor.
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let sink = std::sync::Arc::new(LifecycleOutput::default());
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    let out = exec_confined_command(
        // Use an external executable under every engine. Bare `echo` is a
        // Brush builtin and therefore correctly needs no exec grant.
        "/bin/echo retry-visible",
        &ws.path().to_string_lossy(),
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        &[],
        None,
        &mut Some(&mut gate),
        false,
        None,
        Some(sink.clone()),
        &mut display,
    )
    .await
    .0;

    assert!(out.contains("retry-visible"), "retry result: {out}");
    assert_eq!(gate.asks.len(), 1, "permission prompt count");
    let events = sink.0.lock().unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter(|event| event.starts_with("start:"))
        .cloned()
        .collect();
    assert_eq!(starts.len(), 2, "one viewport per attempt: {events:?}");
    let first_generation = starts[0].trim_start_matches("start:");
    let retry_start = events
        .iter()
        .position(|event| event == &starts[1])
        .expect("retry start event");
    assert!(
        events[..retry_start]
            .iter()
            .any(|event| event == &format!("finish:{first_generation}")),
        "retry started before the denied generation finished: {events:?}"
    );
    let second_generation = starts[1].trim_start_matches("start:");
    assert!(
        events.iter().any(|event| {
            event.starts_with(&format!("write:{second_generation}:"))
                && event.contains("retry-visible")
        }),
        "retry bytes were not delivered to its generation: {events:?}"
    );
    let expected_finish = format!("finish:{second_generation}");
    assert_eq!(events.last(), Some(&expected_finish), "events: {events:?}");
}

/// #2541 round 2 item 2 (red first): the double-indirection signature
/// (`&mut Option<&mut dyn PermissionGate>`, see the doc comment on
/// `exec_confined_command`'s `permission_gate` param) exists so a caller can
/// reborrow the SAME gate for a second sequential confined call — F19's
/// `lifecycle action=run` escalation into `action=build` does exactly that.
/// `exec_confined_command`'s denial-retry used to `permission_gate.take()`
/// the Option permanently empty on ANY allowed denial-recovery, so a second
/// call through the same `Option` saw `None` and silently refused instead of
/// asking — the operator who had just said yes to the first prompt got a
/// denial for the second with no prompt at all. Prove the Option survives: a
/// second `exec_confined_command` call through the SAME
/// `&mut Option<&mut dyn PermissionGate>` still has a live gate to ask.
#[cfg(not(windows))]
#[tokio::test]
async fn a_denial_grant_leaves_the_gate_available_for_a_second_confined_call() {
    let _l = super::disable_ocap_tests::env_lock().await;
    let _eng = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let denied = Caveats {
        exec: Scope::none(),
        // Isolate gate reuse from macOS's unsupported network floor.
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let mut permission_gate: Option<&mut dyn super::PermissionGate> = Some(&mut gate);
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 3, false);

    let first = exec_confined_command(
        "/bin/echo first-call",
        &ws.path().to_string_lossy(),
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        &[],
        None,
        &mut permission_gate,
        false,
        None,
        None,
        &mut display,
    )
    .await;
    assert!(first.0.contains("first-call"), "{}", first.0);
    assert!(
        permission_gate.is_some(),
        "the gate must survive a denial-allow so a second confined call can reuse it"
    );

    let second = exec_confined_command(
        "/bin/echo second-call",
        &ws.path().to_string_lossy(),
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        &[],
        None,
        &mut permission_gate,
        false,
        None,
        None,
        &mut display,
    )
    .await;
    assert!(second.0.contains("second-call"), "{}", second.0);
    assert_eq!(
        gate.asks.len(),
        2,
        "the same gate must be asked for BOTH confined calls, never silently \
         refused because it was consumed by the first"
    );
}

/// #2681 regression: an exec denial is #2628/#2636-replay-eligible exactly
/// like an FS denial already is. Before the fix, `single_grant_covers_missing`
/// and `pending_rerun` population were wired for `FsRead`/`FsWrite` only (see
/// `approved_request_permissions_reruns_denied_run_command` above) — an exec
/// denial never populated `pending_rerun` at all, so an operator's `AllowOnce`
/// answer to the model's `request_permissions(capability="exec", …)` could
/// only ever produce "Retry the original operation now", forcing the model to
/// reissue the identical `run_command` call itself. Each such manual re-issue
/// is a FRESH, independent denial/grant round-trip — exactly the "intermittent
/// denial" pattern #2681 reports (a retest transcript: 129 prompts, 14+ for
/// `git`, before the model gave up).
///
/// Round 1 pinned the issue's literal ask against a `&&`-chained command
/// that execs the SAME out-of-scope program TWICE. Round 3 (P1) narrows
/// replay-eligibility to a single simple command only (see
/// `exec_denial_is_replay_safe`'s doc comment) — a `&&`-chain is no longer
/// eligible regardless of which target is denied or repeated, so that
/// scenario moved to `exec_replay_excluded_for_compound_and_pipeline_shapes`
/// below, which proves it now gets "Retry the original operation" instead.
/// This test keeps the single-command case: the fix this issue is actually
/// about (an exec denial getting a `pending_rerun` slot at all).
#[cfg(unix)]
#[tokio::test]
async fn approved_request_permissions_reruns_denied_run_command_for_exec() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    let ws = tempfile::tempdir().unwrap();
    let workspace = ws.path().canonicalize().unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    // `/bin/echo` is an external program under every engine (bare `echo` is a
    // Brush builtin and needs no exec grant) — denied by an empty exec scope.
    let base = Caveats {
        exec: Scope::none(),
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(&workspace)
    };

    // ── Step 1: run_command denied — exec not granted ───────────────────────
    let mut deny_gate = MockGate::new(false, &base);
    let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
    let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
    let result1 = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "/bin/echo hello"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut deny_gate as &mut dyn PermissionGate),
            execution: Some(&execution1),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        execution1.get(),
        Some(&ExecOutcome::Denied),
        "run_command must be denied: {result1}"
    );
    assert!(
        pending_rerun.is_some(),
        "#2681: an exec denial must be stored in pending_rerun exactly like an \
         FS denial already is: {result1}"
    );

    // ── Step 2: request_permissions approved for the ACTUAL bound target ────
    let mut allow_gate = MockGate::new(true, &base);
    let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
    let result2 = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({
            "capability": "exec",
            "target": "/bin/echo",
            "reason": "#2681 regression test",
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut allow_gate as &mut dyn PermissionGate),
            execution: Some(&execution2),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        pending_rerun.is_none(),
        "#2681: pending_rerun must be consumed after approval: {result2}"
    );
    assert!(
        !result2.contains("Retry the original operation"),
        "#2681: harness must re-run the command directly, not instruct the model \
         to retry it itself: {result2}"
    );
    assert_eq!(
        execution2.get(),
        Some(&ExecOutcome::Passed),
        "#2681: the replay must actually succeed: {result2}"
    );
    assert!(result2.contains("hello"), "{result2}");
}

/// #2681 round 2/3 (P1): a returned exec denial does not mean nothing ran —
/// an earlier command in the SAME `&&`/`;`-chain may already have taken
/// effect (a permitted external program, or a builtin, which always runs
/// regardless of exec caveats) before the denied spawn was even attempted.
/// Measured directly: `/bin/mkdir {dir} && /bin/echo external`, denied on
/// `/bin/echo`, does NOT create `{dir}` even though `/bin/mkdir` is
/// permitted — newt's own exec-authority admission validates the WHOLE
/// command before dispatch, so nothing here partially executes (confirmed
/// with `created_dir.is_dir() == false` after the denial). That atomicity is
/// an engine property, not a guarantee `single_grant_covers_missing` can see
/// or rely on — if a future engine or dispatch path ever weakens it, blindly
/// replaying the whole command on grant would silently re-run `/bin/mkdir`.
///
/// Round 3 (P2 review finding): this test no longer claims to have measured
/// that atomicity across BOTH the `safe-subset` and `brush` engines — round
/// 2's three throwaway probes exercised only `safe-subset` (every unit test
/// forces it; `brush_cwd_error_preserves_denied_execution_class` is the one
/// test that reaches a real `BrushShellTool` directly, by constructing its
/// own registry rather than going through this module's dispatch), so an
/// "equally atomic under brush" claim was never actually established. It
/// does not need to be: `exec_denial_is_replay_safe`'s round-3 rule rejects
/// ANY multi-command shape outright (`/bin/echo` here is the SECOND of TWO
/// inventoried commands, full stop), so this test's outcome does not depend
/// on which engine ran it or on any atomicity property of either one.
#[cfg(unix)]
#[tokio::test]
async fn approved_exec_replay_never_reruns_an_earlier_permitted_command() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    let ws = tempfile::tempdir().unwrap();
    let workspace = ws.path().canonicalize().unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    let created_dir = workspace.join("created_once");
    let created_dir_str = created_dir.to_string_lossy().into_owned();

    // `/bin/mkdir` is PERMITTED; `/bin/echo` (the second, denied command) is not.
    let base = Caveats {
        exec: Scope::only(["/bin/mkdir".to_string()]),
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(&workspace)
    };
    let cmd = format!("/bin/mkdir {created_dir_str} && /bin/echo external");

    // ── Step 1: run_command denied on /bin/echo — /bin/mkdir already ran ───
    let mut deny_gate = MockGate::new(false, &base);
    let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
    let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
    let result1 = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": cmd}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut deny_gate as &mut dyn PermissionGate),
            execution: Some(&execution1),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        execution1.get(),
        Some(&ExecOutcome::Denied),
        "run_command must be denied: {result1}"
    );
    assert!(
        pending_rerun.is_some(),
        "setup: an exec denial must still populate pending_rerun: {result1}"
    );
    assert!(
        !created_dir.is_dir(),
        "ground truth: newt's own admission check already refuses the WHOLE \
         command when any of its exec targets lacks authority, so /bin/mkdir \
         never partially ran here — this is what makes exec_denial_is_replay_safe's \
         multi-command exclusion a defensive narrowing rather than today's \
         load-bearing fix: {result1}"
    );

    // ── Step 2: request_permissions approved for the ACTUAL bound target ────
    let mut allow_gate = MockGate::new(true, &base);
    let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
    let result2 = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({
            "capability": "exec",
            "target": "/bin/echo",
            "reason": "#2681 round 2 regression test",
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut allow_gate as &mut dyn PermissionGate),
            execution: Some(&execution2),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        pending_rerun.is_none(),
        "the pending slot must still be consumed even when replay is declined: {result2}"
    );
    assert!(
        result2.contains("Retry the original operation"),
        "#2681: a compound (non-single-command) exec denial must NOT \
         auto-replay — the model must reissue run_command itself: {result2}"
    );
    assert_ne!(
        execution2.get(),
        Some(&ExecOutcome::Passed),
        "the harness must not have executed a replay at all: {result2}"
    );
}

/// #2681 round 3 (P1 review finding): round 2's leading-spawn check kept TWO
/// shapes wrongly replay-eligible as long as the denied target was the
/// textually-first command — both measured here as eligible under round 2's
/// code and confirmed NOT eligible under round 3's single-simple-command
/// rule:
/// - a `&&`-chained command that execs the SAME target TWICE (round 1's own
///   fixture — `approved_request_permissions_reruns_denied_run_command_for_exec`
///   used to pin this as the issue's literal ask; see that test's doc
///   comment for where the claim moved);
/// - a pipeline whose FIRST stage is the denied target (the review's named
///   "pipeline sibling": pipeline stages start together, not in sequence,
///   so "first in the flattened inventory" says nothing about what the
///   OTHER stage has already done by the time the first one is denied).
///
/// Both deny on exactly `{Exec, "/bin/echo"}` — asserted directly against
/// `pending_rerun.missing` — so `single_grant_covers_missing` ALONE would
/// already say a grant for `/bin/echo` covers the denial; only
/// `exec_denial_is_replay_safe`'s multi-command rejection stands between
/// that and an unsound auto-replay.
#[cfg(unix)]
#[tokio::test]
async fn exec_replay_excluded_for_compound_and_pipeline_shapes() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    for cmd in [
        "/bin/echo one && /bin/echo two",
        "/bin/echo denied | /bin/true",
    ] {
        let ws = tempfile::tempdir().unwrap();
        let workspace = ws.path().canonicalize().unwrap();
        let workspace_str = workspace.to_string_lossy().into_owned();
        let base = Caveats {
            exec: Scope::none(),
            #[cfg(target_os = "macos")]
            net: Scope::All,
            ..caveats_rw(&workspace)
        };

        // ── run_command denied — exec not granted ───────────────────────────
        let mut deny_gate = MockGate::new(false, &base);
        let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
        let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
        execute_tool_with_collaborators(
            "run_command",
            &serde_json::json!({"command": cmd}),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut deny_gate as &mut dyn PermissionGate),
                execution: Some(&execution1),
                pending_rerun: Some(&mut pending_rerun),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(execution1.get(), Some(&ExecOutcome::Denied), "{cmd}");
        let missing = &pending_rerun
            .as_ref()
            .expect("setup: the denial must populate pending_rerun")
            .missing;
        assert_eq!(
            missing
                .iter()
                .map(|r| (r.kind, r.target.as_str()))
                .collect::<Vec<_>>(),
            vec![(DenialKind::Exec, "/bin/echo")],
            "setup: {cmd:?} must deny on exactly /bin/echo, not a narrower or \
             wider set — otherwise this case does not exercise \
             exec_denial_is_replay_safe at all: {missing:?}"
        );

        // ── request_permissions approved for the ACTUAL bound target ───────
        let mut allow_gate = MockGate::new(true, &base);
        let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
        let result2 = execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({
                "capability": "exec",
                "target": "/bin/echo",
                "reason": "#2681 round 3 regression test",
            }),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut allow_gate as &mut dyn PermissionGate),
                execution: Some(&execution2),
                pending_rerun: Some(&mut pending_rerun),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(pending_rerun.is_none(), "{cmd:?}: {result2}");
        assert!(
            result2.contains("Retry the original operation"),
            "{cmd:?} must NOT auto-replay under the single-simple-command rule: {result2}"
        );
        assert_ne!(
            execution2.get(),
            Some(&ExecOutcome::Passed),
            "{cmd:?}: {result2}"
        );
    }
}

/// #2681 round 3 (P1): a direct, fully-mocked unit test of
/// `exec_denial_is_replay_safe` itself, covering shapes the end-to-end
/// dispatch cannot reach under the test harness's `safe-subset` engine — in
/// particular the review's OTHER named measured red, command substitution
/// (`/bin/echo "$(touch marker)"`), which `safe-subset` refuses outright as
/// an unsupported construct before any capability check even runs (so
/// `pending_rerun` is never populated at all under the real dispatch — see
/// `exec_replay_excluded_for_compound_and_pipeline_shapes` for the two
/// shapes that ARE reachable there). Exercising the predicate directly is
/// cheaper and more precise than contriving an end-to-end path for every
/// shape it has to reject.
#[test]
fn exec_denial_is_replay_safe_rejects_every_unsound_shape() {
    use crate::agentic::tools::exec_denial_is_replay_safe;

    // Sound: a single simple command naming exactly the denied target.
    assert!(exec_denial_is_replay_safe("/bin/echo", "/bin/echo hello"));
    // A harmless read redirect/fd duplication is not a mutating effect.
    assert!(exec_denial_is_replay_safe(
        "/bin/echo",
        "/bin/echo hello < /dev/null 2>&1"
    ));

    // Unsound: dynamic content (the review's other named measured red).
    assert!(!exec_denial_is_replay_safe(
        "/bin/echo",
        r#"/bin/echo "$(touch marker)""#
    ));
    // Unsound: a pipeline (stages start together, not in source order).
    assert!(!exec_denial_is_replay_safe(
        "/bin/echo",
        "/bin/echo denied | /bin/true"
    ));
    // Unsound: a `&&`-chain, even when both stages name the SAME target.
    assert!(!exec_denial_is_replay_safe(
        "/bin/echo",
        "/bin/echo one && /bin/echo two"
    ));
    // Unsound: a for-loop's single flattened body entry runs once PER
    // iteration, not once.
    assert!(!exec_denial_is_replay_safe(
        "/bin/echo",
        "for x in a b c; do /bin/echo \"$x\"; done"
    ));
    // Unsound: `&` backgrounding — only a `warnings` entry marks it, with
    // `commands.len() == 1` otherwise, so this pins that `warnings` alone
    // (not just command count) must gate eligibility.
    assert!(!exec_denial_is_replay_safe("/bin/echo", "/bin/echo hi &"));
    // Unsound: a write-shaped redirect can mutate state independent of
    // whether the command it decorates ever actually spawns.
    assert!(!exec_denial_is_replay_safe(
        "/bin/echo",
        "/bin/echo hello > /tmp/marker"
    ));
    // Unsound: malformed shell syntax fails closed, not open.
    assert!(!exec_denial_is_replay_safe("/bin/echo", "/bin/echo '"));
    // Unsound: the inventoried program is not the denied target at all.
    assert!(!exec_denial_is_replay_safe("/bin/echo", "/bin/true"));
}

/// #2681 round 2 (P2): the retest's actual `git add`/`git commit` loop (a
/// retest transcript: an exact `request_permissions(exec, "/usr/bin/git")`
/// grant followed immediately by the SAME denial, repeatedly) is NOT this
/// PR's mechanism.
/// Reproduced here through the real dispatch with a `GitTool` collaborator
/// whose `native_commit_policy()` is `Some` (exactly what makes
/// `tools.rs`'s `commit_requested`/`commit_broker` detection real, per
/// `native_git::needs_commit_broker`): a denied `git commit`-shaped command
/// takes `shell.rs:1230`'s broker early return, which reports the structured
/// denial WITHOUT ever calling `PermissionGate::ask`/`ask_with_caveats` —
/// contrast the plain (non-broker) exec denial below, which DOES consult the
/// gate inline (the pre-existing #263/#905 path, unrelated to #2681).
///
/// This matters because newt-tui's `pending_once_grants` queue (what an
/// operator's `AllowOnce` answer to `request_permissions` populates, per
/// `newt-tui/src/permissions.rs:2125-2133`) is consumed ONLY from inside
/// `ask`/`ask_with_caveats`'s `take_pending_once` call — never from
/// `refresh_caveats`/`mint` (`newt-tui/src/permissions.rs:1853-1858`, which
/// folds in only `recalled_grants`, i.e. session/durable grants). A
/// broker-bearing command's manual retry takes the SAME never-asks-the-gate
/// branch, so an `AllowOnce` grant for it is queued and then structurally
/// unreachable — which is exactly "granted… Retry the original operation
/// now" followed by the identical denial, repeated, in the retest.
///
/// This is a PRE-EXISTING (#905-era) newt-tui/newt-core interaction, not
/// #2681/#2636's `PendingRerun` cross-call machinery (which this PR's other
/// tests cover), and not #2682 (that issue is the FS-write axis — a
/// worktree's common `.git` dir outside `fs_write` — a different mechanism
/// again). It needs a newt-tui-side fix (`refresh_caveats`/`mint` folding in
/// `pending_once_grants`, or the broker branch consulting the gate before
/// its early return) that is out of scope for this newt-core-only PR; see
/// RESULT.md. `Fixes #2681` does NOT cover this retest failure.
#[cfg(unix)]
#[tokio::test]
async fn broker_bearing_commit_denial_never_consults_the_gate() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    struct NoSignPolicy;
    impl agent_toolchain::native_git::CommitPolicy for NoSignPolicy {
        fn finalize_message(&self, message: &str) -> Result<String, String> {
            Ok(message.to_string())
        }
        fn signing_required(&self) -> bool {
            false
        }
        fn sign_commit(&self, _payload: &[u8]) -> Result<String, String> {
            Err("signing not required by this test".into())
        }
        fn committed(&self) {}
    }

    struct StubGitTool;
    impl crate::agentic::git_tool::GitTool for StubGitTool {
        fn native_commit_policy(
            &self,
        ) -> Option<std::sync::Arc<dyn agent_toolchain::native_git::CommitPolicy>> {
            Some(std::sync::Arc::new(NoSignPolicy))
        }
        fn dispatch(
            &self,
            _op: &str,
            _args: &serde_json::Value,
            _caveats: &crate::git_caveats::GitCaveats,
            _session: &Caveats,
        ) -> Result<String, String> {
            Err("not used by this test".into())
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace = ws.path().canonicalize().unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    // #2681 round 3: the hermetic constructor (env_clear + a private HOME),
    // not a bare `git` that inherits this process's GIT_DIR/HOME/hooks.
    let home = tempfile::tempdir().unwrap();
    assert!(
        super::super::tests::git_shell_grant::hermetic_git(&workspace, home.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(workspace.join("file.txt"), b"hello").unwrap();

    let base = Caveats {
        exec: Scope::none(),
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(&workspace)
    };
    let git_tool = StubGitTool;

    // ── A broker-bearing (git commit) denial never asks the gate ───────────
    let mut deny_gate = MockGate::new(false, &base);
    let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
    let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
    let result1 = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "git add -A && git commit -q -m probe"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut deny_gate as &mut dyn PermissionGate),
            execution: Some(&execution1),
            pending_rerun: Some(&mut pending_rerun),
            git_tool: Some(&git_tool),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(execution1.get(), Some(&ExecOutcome::Denied), "{result1}");
    assert!(
        pending_rerun.is_none(),
        "a broker-bearing exec denial must never populate pending_rerun \
         (commit_broker_used excludes it, by design): {result1}"
    );
    assert!(
        deny_gate.asks.is_empty(),
        "the broker early return (shell.rs:1230) must never consult the \
         permission gate on denial — an AllowOnce grant queued elsewhere \
         (newt-tui's pending_once_grants) has no path to be consumed here: \
         asks={:?}",
        deny_gate.asks
    );
    let staged = super::super::tests::git_shell_grant::hermetic_git(&workspace, home.path())
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(
        staged.stdout.is_empty(),
        "nothing partially ran either — `git add` never staged anything: {}",
        String::from_utf8_lossy(&staged.stdout)
    );

    // ── Contrast: a plain (non-broker) exec denial DOES ask the gate ───────
    let mut plain_deny_gate = MockGate::new(false, &base);
    let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
    let _ = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "/bin/echo plain"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut plain_deny_gate as &mut dyn PermissionGate),
            execution: Some(&execution2),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(execution2.get(), Some(&ExecOutcome::Denied));
    assert!(
        !plain_deny_gate.asks.is_empty(),
        "the pre-existing #263/#905 inline path DOES consult the gate for a \
         non-broker exec denial — the asymmetry above is specific to the \
         broker branch, not a general 'the gate is never asked' fact"
    );
}

/// Grounds exact-target prompt tests in a real Seatbelt process-exec rule.
/// A harmless test executable is reached through temporary non-system symlinks;
/// granting one must launch it without granting its same-named sibling.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn exact_executable_grants_launch_only_the_approved_non_system_binary() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = super::disable_ocap_tests::EnvVar::unset("NEWT_FULL_ACCESS");
    let ws = tempfile::tempdir().unwrap();
    let root = ws.path().canonicalize().unwrap();
    let approved = root.join("approved");
    let other = root.join("other");
    std::fs::create_dir(&approved).unwrap();
    std::fs::create_dir(&other).unwrap();
    let executable = approved.join("python");
    let sibling = other.join("python");
    let image = std::env::current_exe().unwrap().canonicalize().unwrap();
    std::os::unix::fs::symlink(&image, &executable).unwrap();
    std::os::unix::fs::symlink(&image, &sibling).unwrap();
    const CHILD: &str =
        "agentic::tools::execute_tool_branch_tests::permissions::exact_executable_child";
    let control = std::process::Command::new(&executable)
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .output()
        .unwrap();
    assert!(control.status.success(), "unconfined fixture must work");
    assert!(String::from_utf8_lossy(&control.stdout)
        .lines()
        .any(|line| line == "EXACT_EXEC_OK"));
    let base = Caveats {
        fs_read: Scope::only([
            root.to_string_lossy().into_owned(),
            image.to_string_lossy().into_owned(),
        ]),
        fs_write: Scope::none(),
        // This tests exact exec authority, not unsupported macOS network rules.
        net: Scope::All,
        ..caveats_rw(&root)
    };
    let mut gate = MockGate::new(true, &base);
    let command = format!(
        "'{}' --exact {CHILD} --ignored --nocapture",
        executable.display()
    );
    let result = run_tool_gated(
        "run_command",
        serde_json::json!({"command": command}),
        &root,
        &base,
        &mut gate,
    )
    .await;
    assert!(
        result.lines().any(|line| line == "EXACT_EXEC_OK"),
        "real sandbox result: {result}"
    );
    assert!(
        !result.starts_with("error:"),
        "child must exit successfully: {result}"
    );
    assert_eq!(
        gate.asks,
        vec![(
            "run_command".into(),
            format!("exec:{}", executable.display())
        )]
    );

    let granted = crate::agentic::widen_caveats(
        &base,
        &[(DenialKind::Exec, executable.to_string_lossy().into_owned())],
    );
    let denied = run_tool(
        "run_command",
        serde_json::json!({"command": format!("'{}' WRONG_BINARY", sibling.display())}),
        &root,
        &granted,
        None,
    )
    .await;
    assert!(denied.starts_with("capability denied:"), "{denied}");
    // The same one-call policy does not change the next invocation's baseline.
    let again = run_tool(
        "run_command",
        serde_json::json!({"command": command}),
        &root,
        &base,
        None,
    )
    .await;
    assert!(again.starts_with("capability denied:"), "{again}");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "private child entrypoint for the real exact-executable grant test"]
fn exact_executable_child() {
    println!("EXACT_EXEC_OK");
}

#[test]
fn live_permission_refresh_defaults_to_the_supplied_baseline() {
    let baseline = Caveats {
        fs_read: Scope::only(["workspace".into()]),
        fs_write: Scope::none(),
        exec: Scope::only(["head".into()]),
        net: Scope::none(),
        max_calls: CountBound::AtMost(3),
        valid_for_generation: Scope::only([7]),
    };
    let mut gate = MockGate::new(false, &Caveats::top());
    let PermissionDecision::Allow(current) = gate.refresh_caveats(&baseline) else {
        panic!("an unchanged gate must retain the caller's baseline");
    };
    assert_eq!(current, baseline);
    assert!(gate.asks.is_empty(), "refresh must not ask the operator");
}

#[test]
fn native_once_filesystem_schema_and_acknowledgement_explain_the_retry() {
    let directory = tempfile::tempdir().unwrap();
    let absolute = directory
        .path()
        .join("quoted \"file\"")
        .to_string_lossy()
        .into_owned();
    let nul = format!("{absolute}\0");
    let definitions = tool_definitions();
    let command = definitions
        .as_array()
        .unwrap()
        .iter()
        .find(|definition| definition["function"]["name"] == "run_command")
        .unwrap();
    for axis in ["fs_read", "fs_write"] {
        let property = &command["function"]["parameters"]["properties"][axis];
        assert_eq!(property["type"], "array", "{axis}: {property}");
        assert_eq!(property["items"]["type"], "string");
        for capability in [axis, if axis == "fs_read" { "read" } else { "write" }] {
            for (target, native_hint) in
                [(&*absolute, true), ("relative.txt", false), (&*nul, false)]
            {
                let args = serde_json::json!({"capability": capability, "target": target});
                let mut gate = MockGate::new(true, &Caveats::top());
                let result = execute_request_permissions(
                    &args,
                    Some(&mut gate),
                    false,
                    20,
                    "/workspace",
                    None,
                )
                .2;
                assert!(permission_grant_succeeded(
                    "request_permissions",
                    &args,
                    true,
                    &result,
                    None
                ));
                assert_eq!(result.contains("run_command"), native_hint, "{result:?}");
                if native_hint {
                    assert!(
                        result.contains(&format!("{axis}={}", serde_json::json!([target]))),
                        "{result}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn native_once_filesystem_rejects_the_whole_invalid_declaration_before_prompting() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let workspace = tempfile::tempdir().unwrap();
    let baseline = caveats_rw(workspace.path());
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!("/approved/file"),
        serde_json::json!(["/approved/file", 7]),
        serde_json::json!(["/approved/file", ""]),
        serde_json::json!(["/approved/file", "relative/file"]),
        serde_json::json!(["/approved/file", "bad\u{0}path"]),
    ] {
        let mut gate = MockGate::new(false, &baseline);
        let result = run_tool_gated(
            "run_command",
            serde_json::json!({
                "command": "/bin/echo MUST_NOT_RUN",
                "fs_read": [workspace.path().join("input")],
                "fs_write": invalid,
            }),
            workspace.path(),
            &baseline,
            &mut gate,
        )
        .await;
        assert!(
            gate.asks.is_empty(),
            "invalid declarations must not consume or prompt any grant: {:?}",
            gate.asks
        );
        assert!(
            result.starts_with("error: run_command fs_write"),
            "{result}"
        );
        assert!(!result.lines().any(|line| line == "MUST_NOT_RUN"));
    }
}

/// Grounds complete-declaration admission with a real workspace side effect
/// that must not occur when either grant decision drops a declared path.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_once_filesystem_incomplete_decisions_never_start_the_child() {
    use crate::caveats::Scope;
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = super::disable_ocap_tests::EnvVar::unset("NEWT_FULL_ACCESS");
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let covered = outside.path().canonicalize().unwrap().join("covered.txt");
    let added = covered.with_file_name("added.txt");
    std::fs::write(&covered, "covered").unwrap();
    std::fs::write(&added, "added").unwrap();
    for stage in ["initial", "retry"] {
        let marker = root.join(stage);
        let mut baseline = crate::confined_exec::workspace_confined_caveats(&root);
        baseline.fs_read = Scope::only([
            root.to_string_lossy().into_owned(),
            covered.to_string_lossy().into_owned(),
        ]);
        baseline.exec = if stage == "initial" {
            Scope::only(["/usr/bin/touch".into()])
        } else {
            Scope::none()
        };
        let mut gate_base = baseline.clone();
        if stage == "initial" {
            gate_base.fs_read = Scope::only([root.to_string_lossy().into_owned()]);
        }
        let mut gate = MockGate::new(true, &gate_base);
        // Grounds the whole-manifest check with a real side effect: this child
        // could touch the workspace even after a gate drops declared FS reads.
        let result = run_tool_gated(
            "run_command",
            serde_json::json!({
                "command": format!("/usr/bin/touch '{}'", marker.display()),
                "fs_read": [covered, added],
            }),
            &root,
            &baseline,
            &mut gate,
        )
        .await;
        assert!(
            !marker.exists(),
            "{stage}: incomplete declaration reached child: {result}"
        );
        assert!(result.starts_with("capability denied:"), "{result}");
        assert_eq!(gate.asks.len(), if stage == "initial" { 1 } else { 2 });
    }
}

/// Grounds declared one-shot policy with a real Seatbelt child that must read
/// one external file and write another, without granting their parent directory.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_once_filesystem_declarations_reach_only_the_admitted_child() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = super::disable_ocap_tests::EnvVar::unset("NEWT_FULL_ACCESS");
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let private = outside.path().canonicalize().unwrap();
    let input = private.join("input.txt");
    let output = private.join("output.txt");
    let sibling = private.join("sibling.txt");
    std::fs::write(&input, "ONE_SHOT_COPY\n").unwrap();
    std::fs::write(&output, "BEFORE\n").unwrap();
    std::fs::write(&sibling, "SIBLING_UNCHANGED\n").unwrap();
    let baseline = Caveats {
        exec: Scope::only(["/bin/cp".into()]),
        // This fixture tests filesystem admission. Restricted network authority
        // is unsupported by the macOS backend and would fail before the copy.
        net: Scope::All,
        ..crate::confined_exec::workspace_confined_caveats(&root)
    };
    let command = format!("/bin/cp '{}' '{}'", input.display(), output.display());
    let declared = serde_json::json!({
        "command": command,
        "fs_read": [input],
        "fs_write": [output],
    });
    let mut gate = MockGate::new(true, &baseline);
    let result = run_tool_gated("run_command", declared.clone(), &root, &baseline, &mut gate).await;
    assert!(
        tool_result_ok(&result),
        "the declared child must run: {result}"
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "ONE_SHOT_COPY\n");
    assert_eq!(
        gate.asks,
        vec![
            ("run_command".into(), format!("fs_read:{}", input.display())),
            (
                "run_command".into(),
                format!("fs_write:{}", output.display())
            ),
        ],
    );
    std::fs::write(&output, "RESET\n").unwrap();
    let undeclared = run_tool_gated(
        "run_command",
        serde_json::json!({"command": command}),
        &root,
        &baseline,
        &mut gate,
    )
    .await;
    assert!(!tool_result_ok(&undeclared), "{undeclared}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "RESET\n");
    assert_eq!(
        gate.asks.len(),
        2,
        "an undeclared invocation consumes nothing"
    );
    let wrong_target = run_tool_gated(
        "run_command",
        serde_json::json!({
            "command": format!("/bin/cp '{}' '{}'", input.display(), sibling.display()),
            "fs_read": [input],
            "fs_write": [output],
        }),
        &root,
        &baseline,
        &mut gate,
    )
    .await;
    assert!(!tool_result_ok(&wrong_target), "{wrong_target}");
    assert_eq!(
        std::fs::read_to_string(&sibling).unwrap(),
        "SIBLING_UNCHANGED\n"
    );
    gate.allow = false;
    let second = run_tool_gated("run_command", declared, &root, &baseline, &mut gate).await;
    assert!(second.starts_with("capability denied:"), "{second}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "RESET\n");
    assert_eq!(
        std::fs::read_to_string(&sibling).unwrap(),
        "SIBLING_UNCHANGED\n"
    );
    assert_eq!(
        gate.asks.len(),
        6,
        "the next declaration needs a new decision"
    );
}

/// Grounds the live-policy seam and exact session-grant tests in a real native
/// child's Seatbelt file read. The baseline stays unchanged across calls: only
/// the gate remembers approval, and the sibling remains outside its scope.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn live_permission_refresh_reaches_native_child_in_the_same_turn() {
    use crate::agentic::RepeatCallGuard;

    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = super::disable_ocap_tests::EnvVar::unset("NEWT_FULL_ACCESS");
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let approved = private.path().canonicalize().unwrap().join("approved.txt");
    let sibling = approved.with_file_name("sibling.txt");
    std::fs::write(&approved, "NATIVE_SESSION_READ\n").unwrap();
    std::fs::write(&sibling, "UNGRANTED_SIBLING\n").unwrap();
    let control = std::process::Command::new("/usr/bin/head")
        .args(["-n", "1"])
        .arg(&approved)
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "fixture must be readable before confinement"
    );
    assert_eq!(control.stdout, b"NATIVE_SESSION_READ\n");

    struct SessionGate {
        current: Caveats,
        prompts: usize,
    }
    impl PermissionGate for SessionGate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            assert!(requests.iter().all(|r| r.tool == "request_permissions"));
            self.prompts += 1;
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            self.current = crate::agentic::widen_caveats(&self.current, &grants);
            PermissionDecision::Allow(self.current.clone())
        }
        fn refresh_caveats(&mut self, _: &Caveats) -> PermissionDecision {
            PermissionDecision::Allow(self.current.clone())
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }
    let baseline = Caveats {
        exec: Scope::only(["/usr/bin/head".into()]),
        // Reach the filesystem denial; macOS cannot admit restricted networking.
        net: Scope::All,
        ..crate::confined_exec::workspace_confined_caveats(&root)
    };
    let mut gate = SessionGate {
        current: baseline.clone(),
        prompts: 0,
    };
    let command =
        serde_json::json!({"command": format!("/usr/bin/head -n 1 '{}'", approved.display())});
    let execution = std::sync::OnceLock::new();
    let denied = execute_tool_with_collaborators(
        "run_command",
        &command,
        &root.to_string_lossy(),
        false,
        20,
        &baseline,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate),
            execution: Some(&execution),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        denied.contains("Operation not permitted"),
        "native denial: {denied}"
    );
    assert!(!denied.contains("NATIVE_SESSION_READ"));
    assert_eq!(gate.prompts, 0, "native stderr is not a permission request");
    assert_eq!(execution.get(), Some(&ExecOutcome::Failed));
    let mut repeats = RepeatCallGuard::default();
    repeats.record(
        "run_command",
        &command,
        tool_result_ok(&denied),
        &denied,
        execution.get().copied(),
        ReadScope {
            workspace: &root.to_string_lossy(),
            caveats: &baseline,
        },
    );
    assert!(repeats.repeat_steer("run_command", &command).is_some());

    let permission = serde_json::json!({"capability": "fs_read", "target": approved});
    let grant = run_tool_gated(
        "request_permissions",
        permission.clone(),
        &root,
        &baseline,
        &mut gate,
    )
    .await;
    assert!(grant.starts_with("granted:"), "{grant}");
    repeats.record(
        "request_permissions",
        &permission,
        tool_result_ok(&grant),
        &grant,
        None,
        ReadScope {
            workspace: &root.to_string_lossy(),
            caveats: &baseline,
        },
    );
    assert!(
        repeats.repeat_steer("run_command", &command).is_none(),
        "the real native failure must reach refreshed confinement after a grant"
    );
    let allowed = run_tool_gated("run_command", command, &root, &baseline, &mut gate).await;
    assert!(
        allowed.lines().any(|line| line == "NATIVE_SESSION_READ"),
        "the same-turn native child must receive the session grant: {allowed}"
    );
    let other = run_tool_gated(
        "run_command",
        serde_json::json!({"command": format!("/usr/bin/head -n 1 '{}'", sibling.display())}),
        &root,
        &baseline,
        &mut gate,
    )
    .await;
    assert!(
        other.contains("Operation not permitted"),
        "sibling must remain denied: {other}"
    );
    assert!(!other.contains("UNGRANTED_SIBLING"));
    assert_eq!(gate.prompts, 1, "only the explicit request prompts");
    assert!(!baseline.permits_fs_read(&approved.to_string_lossy()));
}

/// Gate denies → the result is the standard denial, bit-for-bit equal to
/// the no-gate path (#263: deny = the current denial result).
#[tokio::test]
async fn gate_deny_keeps_the_standard_denial_bit_for_bit() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(false, &denied);
    let gated = run_tool_gated(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    let ungated = run_tool(
        "read_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(gated, ungated);
    assert_eq!(
        gated,
        denied_fs_result("fs_read", &ws.path().join("secret.txt").to_string_lossy())
    );
    assert_eq!(gate.asks.len(), 1, "the human was asked exactly once");
}

/// Gate allows fs_write denials → write_file, edit_file, and delete_file proceed.
#[tokio::test]
async fn gate_allow_turns_fs_write_denials_into_real_writes() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("f.txt"), "old\n").unwrap();
    std::fs::write(ws.path().join("stale.txt"), "remove me\n").unwrap();
    let denied = Caveats {
        fs_write: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "write_file",
        serde_json::json!({"path": "new.txt", "content": "fresh"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("wrote new.txt"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("new.txt")).unwrap(),
        "fresh"
    );
    let out = run_tool_gated(
        "edit_file",
        serde_json::json!({"path": "f.txt", "old_string": "old", "new_string": "new"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("edited f.txt"), "got: {out}");
    let out = run_tool_gated(
        "delete_file",
        serde_json::json!({"path": "stale.txt"}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("deleted stale.txt"), "got: {out}");
    assert!(
        !ws.path().join("stale.txt").exists(),
        "gate-approved delete must remove the file"
    );
    assert_eq!(gate.asks.len(), 3);
    assert_eq!(gate.asks[0].0, "write_file");
    assert!(
        gate.asks[1].1.starts_with("fs_write:"),
        "got: {:?}",
        gate.asks[1]
    );
    assert_eq!(gate.asks[2].0, "delete_file");
}

/// list_dir consults the gate on an fs_read denial like read_file does.
#[tokio::test]
async fn gate_allow_turns_list_dir_denial_into_the_listing() {
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("seen.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = MockGate::new(true, &denied);
    let out = run_tool_gated(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        &mut gate,
    )
    .await;
    assert!(out.contains("seen.txt"), "got: {out}");
}

/// A buggy/hostile gate answering Allow with caveats that STILL don't
/// cover the path must not bypass enforcement: the widened authority is
/// re-checked, never assumed (fs_gate_allows' re-check).
#[tokio::test]
async fn gate_allow_without_real_coverage_is_still_denied() {
    struct LyingGate;
    impl super::PermissionGate for LyingGate {
        fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
            // "Allow", but the caveats grant nothing at all.
            super::PermissionDecision::Allow(Caveats {
                fs_read: Scope::none(),
                fs_write: Scope::none(),
                exec: Scope::none(),
                net: Scope::none(),
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            })
        }
        fn ask_question(&mut self, _question: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
    }
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "x").unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..caveats_rw(ws.path())
    };
    let mut gate = LyingGate;
    let out = execute_tool(
        "read_file",
        &serde_json::json!({"path": "secret.txt"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &denied,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(
        out,
        denied_fs_result("fs_read", &ws.path().join("secret.txt").to_string_lossy())
    );
}

#[tokio::test]
async fn declined_or_incomplete_lifecycle_build_launches_nothing() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname=\"denied-build\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let base = crate::confined_exec::workspace_confined_caveats(ws.path());
    for allow in [false, true] {
        let mut gate = MockGate::new(allow, &base);
        let result = run_tool_gated(
            "lifecycle",
            serde_json::json!({"phase":"test", "action":"build"}),
            ws.path(),
            &base,
            &mut gate,
        )
        .await;
        assert!(
            result.starts_with("capability denied: lifecycle action=build"),
            "{result}"
        );
        assert_eq!(gate.asks.len(), 1);
        assert!(gate.asks[0].1.starts_with("build:"));
        assert!(!ws.path().join("target").exists());
        assert!(!ws.path().join("Cargo.lock").exists());
    }
}

/// Grounds the Build grant unit tests in the full tool dispatch: the same
/// workspace cannot run without the gate and runs Cargo/tests after AllowOnce.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
#[tokio::test]
#[ignore = "real Cargo compiler and Seatbelt"]
async fn lifecycle_build_grant_runs_real_cargo() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir(ws.path().join("src")).unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname=\"granted-build\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.path().join("src/lib.rs"),
        "#[test] fn actual_test() { assert_eq!(2 + 2, 4); }",
    )
    .unwrap();
    let base = crate::confined_exec::workspace_confined_caveats(ws.path());
    for allow in [false, true] {
        // Real lifecycle admission uses the canonical workspace in its prompt.
        let canonical = ws.path().canonicalize().unwrap();
        let mut gate = MockGate::new(allow, &crate::confined_exec::build_tool_caveats(&canonical));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            run_tool_gated(
                "lifecycle",
                serde_json::json!({"phase":"test", "action":"build"}),
                ws.path(),
                &base,
                &mut gate,
            ),
        )
        .await
        .expect("real lifecycle build deadline");
        assert_eq!(gate.asks.len(), 1);
        if allow {
            assert!(result.contains("1 passed"), "{result}");
        } else {
            assert!(
                result.starts_with("capability denied: lifecycle action=build"),
                "{result}"
            );
            assert!(!ws.path().join("target").exists());
        }
    }
}

#[cfg(unix)]
async fn command_cwd_fixture_call(
    workspace: &std::path::Path,
    default_cwd: Option<&std::path::Path>,
    args: serde_json::Value,
    caveats: &Caveats,
) -> (String, Option<ExecOutcome>) {
    let execution = std::sync::OnceLock::new();
    let result = execute_tool_with_collaborators(
        "run_command",
        &args,
        workspace.to_str().unwrap(),
        false,
        100,
        caveats,
        &mut NoMcp,
        ToolCollaborators {
            default_command_cwd: default_cwd,
            execution: Some(&execution),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    (result, execution.into_inner())
}

#[cfg(unix)]
#[tokio::test]
async fn command_cwd_default_places_actual_effects_without_changing_authority() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let before = serde_json::to_value(&caveats).unwrap();
    let args = serde_json::json!({"command": "/bin/sh -c 'printf selected > marker'"});
    let original = args.clone();
    for name in ["selected", "selected "] {
        let selected = workspace.join(name);
        std::fs::create_dir(&selected).unwrap();
        let (result, outcome) =
            command_cwd_fixture_call(&workspace, Some(&selected), args.clone(), &caveats).await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{result}");
        assert_eq!(
            std::fs::read_to_string(selected.join("marker")).unwrap(),
            "selected"
        );
    }
    assert!(!workspace.join("marker").exists());
    assert_eq!(args, original);
    assert_eq!(serde_json::to_value(&caveats).unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn command_cwd_explicit_wins_and_leading_cd_is_relative_to_it() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let selected = workspace.join("selected");
    let explicit = workspace.join("explicit");
    std::fs::create_dir(&selected).unwrap();
    std::fs::create_dir_all(explicit.join("child")).unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    for (command, expected) in [
        (
            "/bin/sh -c 'printf explicit > direct'",
            explicit.join("direct"),
        ),
        (
            "cd child && /bin/sh -c 'printf explicit > nested'",
            explicit.join("child/nested"),
        ),
    ] {
        let (result, outcome) = command_cwd_fixture_call(
            &workspace,
            Some(&selected),
            serde_json::json!({"command": command, "cwd": "explicit"}),
            &caveats,
        )
        .await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{result}");
        assert_eq!(std::fs::read_to_string(expected).unwrap(), "explicit");
    }
    assert_eq!(std::fs::read_dir(&selected).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn command_cwd_outside_default_is_denied_before_effects() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().canonicalize().unwrap();
    let workspace = parent.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let (result, outcome) = command_cwd_fixture_call(
        &workspace,
        Some(&parent),
        serde_json::json!({"command": "/bin/sh -c 'printf forbidden > marker'"}),
        &caveats,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Denied), "{result}");
    assert!(!parent.join("marker").exists());
    assert!(!workspace.join("marker").exists());
}

#[tokio::test]
async fn command_cwd_does_not_change_file_tools_or_baseline_read_routing() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _routing = super::disable_ocap_tests::EnvVar::unset("NEWT_NO_ROUTE");
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let selected = workspace.join("selected");
    std::fs::create_dir(&selected).unwrap();
    std::fs::write(workspace.join("marker"), "WORKSPACE_CONTENT").unwrap();
    std::fs::write(selected.join("marker"), "SELECTED_CONTENT").unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([workspace.to_string_lossy().into_owned()]),
        fs_write: Scope::none(),
        exec: Scope::none(),
        ..Caveats::top()
    };
    for (tool, args, default_cwd) in [
        (
            "read_file",
            serde_json::json!({"path": "marker"}),
            Some(selected.as_path()),
        ),
        (
            "run_command",
            serde_json::json!({"command": "cat marker"}),
            None,
        ),
    ] {
        let result = execute_tool_with_collaborators(
            tool,
            &args,
            workspace.to_str().unwrap(),
            false,
            100,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                default_command_cwd: default_cwd,
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.contains("WORKSPACE_CONTENT"), "{result}");
        assert!(!result.contains("SELECTED_CONTENT"), "{result}");
    }
}

/// #2628/#2636 regression: after a `run_command` denial, the operator
/// approving via `request_permissions` must cause the harness to re-run the
/// command with the widened one-shot grant, EXACTLY ONCE, under exactly the
/// authority it was denied on. The model must receive the command result
/// directly — never "Retry the original operation now".
///
/// Grounds: `pending_rerun` wiring through the dispatch arms, plus round1's
/// four P1 findings — binding to the denial, replaying under original
/// policy, one-shot grant consumption, and disclosure of what approval runs.
#[cfg(unix)]
#[tokio::test]
async fn approved_request_permissions_reruns_denied_run_command() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    let outer = tempfile::tempdir().unwrap();
    let workspace_dir = tempfile::tempdir_in(outer.path()).unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    // A target path the command declares it will write (outside workspace so
    // that the workspace-scoped write-scope does not accidentally permit it),
    // owned by its own TempDir rather than a fixed sibling path.
    let target_dir = tempfile::tempdir_in(outer.path()).unwrap();
    let outfile = target_dir.path().join("out_2628.txt");
    let outfile_str = outfile.to_string_lossy().into_owned();
    // `cp` rather than shell redirection (`>`): the safe-subset engine's
    // confined free-form dispatch does not support redirection (verified: a
    // bare `printf … > file` fails "No such file or directory" even with an
    // absolute binary path, while `touch`/`cp` with no redirection succeed).
    let src = target_dir.path().join("src_2628.txt");
    std::fs::write(&src, "HELLO_2628").unwrap();
    let src_str = src.to_string_lossy().into_owned();
    // Pre-created (empty), not newly minted by the replay: an OVERWRITE, not
    // a CREATE — the confined shell's kernel-level fs_write grant is scoped
    // to the leaf path and (verified) does not itself carry the parent
    // directory's create permission a brand-new file would need.
    std::fs::write(&outfile, "").unwrap();

    // Caveats with no fs_write — the declared write request will be denied.
    let base = Caveats {
        fs_write: Scope::none(),
        ..Caveats::top()
    };

    // ── Step 1: run_command denied because it declares fs_write ──────────────
    let mut deny_gate = MockGate::new(false, &base);
    let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
    let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
    let result1 = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/cp {src_str} {outfile_str}"),
            "fs_read": [src_str.clone()],
            "fs_write": [outfile_str.clone()],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut deny_gate as &mut dyn PermissionGate),
            execution: Some(&execution1),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    // Verify the denial was observed and recorded for re-run, and nothing
    // wrote the target yet.
    assert_eq!(
        execution1.get(),
        Some(&ExecOutcome::Denied),
        "run_command must be denied: {result1}"
    );
    assert!(
        pending_rerun.is_some(),
        "#2628: denied run_command must be stored in pending_rerun: {result1}"
    );
    assert_eq!(
        std::fs::read_to_string(&outfile).unwrap(),
        "",
        "denied command must not have run: {result1}"
    );

    // ── finding 1: an UNRELATED tool call in between invalidates the slot ────
    let mut noop_gate = MockGate::new(true, &base);
    let _ = execute_tool_with_collaborators(
        "resume_context",
        &serde_json::json!({}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut noop_gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        pending_rerun.is_none(),
        "#2636 finding 1: an intervening unrelated tool call must invalidate a stale pending_rerun"
    );

    // ── Re-deny to repopulate the slot, then approve a DIFFERENT target —────
    // finding 1: an unrelated grant must not authorize replaying this
    // invocation. This is its own isolated deny→(wrong)approve cycle —
    // `request_permissions` always consumes whatever slot it finds (matched
    // or not), so this deliberately spends and discards it, then the main
    // happy path below re-denies fresh for the correctly-bound approval.
    {
        let mut deny_gate2 = MockGate::new(false, &base);
        let result1b = execute_tool_with_collaborators(
            "run_command",
            &serde_json::json!({
                "command": format!("/bin/cp {src_str} {outfile_str}"),
                "fs_read": [src_str.clone()],
                "fs_write": [outfile_str.clone()],
            }),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut deny_gate2 as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_rerun),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(pending_rerun.is_some(), "{result1b}");

        let unrelated_target = target_dir.path().join("unrelated.txt");
        let mut allow_unrelated_gate = MockGate::new(true, &base);
        let unrelated_result = execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({
                "capability": "fs_write",
                "target": unrelated_target.to_string_lossy(),
                "reason": "unrelated grant",
            }),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut allow_unrelated_gate as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_rerun),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&outfile).unwrap(),
            "",
            "#2636 finding 1: an unrelated grant must not replay the bound denial: {unrelated_result}"
        );
        assert!(
            pending_rerun.is_none(),
            "request_permissions always consumes the slot it finds"
        );
    }

    // ── re-deny fresh, then Step 2: request_permissions approved for the
    // ACTUAL bound target —────
    let mut deny_gate3 = MockGate::new(false, &base);
    let result1c = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/cp {src_str} {outfile_str}"),
            "fs_read": [src_str.clone()],
            "fs_write": [outfile_str.clone()],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut deny_gate3 as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(pending_rerun.is_some(), "{result1c}");
    // harness re-runs the command exactly once.
    let mut allow_gate = MockGate::new(true, &base);
    let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
    let result2 = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({
            "capability": "fs_write",
            "target": outfile_str.clone(),
            "reason": "#2628 regression test",
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut allow_gate as &mut dyn PermissionGate),
            execution: Some(&execution2),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    // The pending_rerun slot must be consumed (re-run happened).
    assert!(
        pending_rerun.is_none(),
        "#2628: pending_rerun must be consumed after approval: {result2}"
    );
    // The model must NOT receive the old "Retry the original operation" message.
    assert!(
        !result2.contains("Retry the original operation"),
        "#2628: harness must re-run the command directly, not instruct the model to retry: {result2}"
    );
    // The command must actually have executed exactly once, with its real effect.
    assert_eq!(
        execution2.get(),
        Some(&ExecOutcome::Passed),
        "#2636 finding 5: replay must actually succeed: {result2}"
    );
    assert_eq!(
        std::fs::read_to_string(&outfile).expect("replay must have written the file"),
        "HELLO_2628",
        "#2636 finding 5: the replay's real effect must be observed directly, not inferred from output text"
    );

    // ── sanity: a later operation is gated fresh, not silently replayed ──────
    // `MockGate` has no `pending_once_grants` queue to reproduce the real
    // finding-2 defect (round1's own note); that mechanism is covered by
    // `consume_pending_once_removes_what_ask_just_queued_for_request_permissions`
    // in newt-tui against the real gate. This only pins that the #2628 slot
    // itself does not leak a second execution.
    let mut second_write_gate = MockGate::new(false, &base);
    let later_result = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/cp {src_str} {outfile_str}"),
            "fs_read": [src_str.clone()],
            "fs_write": [outfile_str.clone()],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut second_write_gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        later_result.starts_with("capability denied:"),
        "#2636 finding 2: a later operation on the same target must be denied again, \
         not silently honor the already-spent allow-once grant: {later_result}"
    );
    assert_eq!(
        std::fs::read_to_string(&outfile).unwrap(),
        "HELLO_2628",
        "the denied second write must not have run"
    );
}

/// #2636 finding 1 (FIX-FIRST): string-based denial capture fires on successful child
/// output. A command that succeeds but prints `UNGRANTED_FS_AUTHORITY_DENIAL` as its
/// stdout must NOT populate the pending_rerun slot — no typed pre-exec denial was issued.
/// Before the fix the string check fires and the slot is populated → assertion red.
#[cfg(unix)]
#[tokio::test]
async fn forged_denial_string_in_stdout_does_not_populate_pending_rerun() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    let outer = tempfile::tempdir().unwrap();
    let workspace = outer.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();

    // A file containing the denial string verbatim — the "forged output" shape.
    let bait = workspace.join("bait.txt");
    std::fs::write(&bait, super::super::shell::UNGRANTED_FS_AUTHORITY_DENIAL).unwrap();
    let bait_str = bait.to_string_lossy().into_owned();

    let base = Caveats {
        fs_read: crate::caveats::Scope::All,
        fs_write: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let mut allow_gate = MockGate::new(true, &base);
    let mut pending_rerun: Option<super::super::PendingRerun> = None;

    let result = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/cat {bait_str}"),
            "fs_read": [bait_str],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut allow_gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    // cat succeeded; the denial string appeared in child stdout, not from a typed denial.
    assert!(
        result.contains(super::super::shell::UNGRANTED_FS_AUTHORITY_DENIAL),
        "cat must output the forged string: {result}"
    );
    assert!(
        pending_rerun.is_none(),
        "#2636 finding 1: successful cat of a forged-string file must not populate \
         pending_rerun (string capture cannot distinguish child output from a real denial)"
    );
}

/// #2636 finding 3 (FIX-FIRST): consume_pending_once must NOT be called when the
/// operator's grant does not cover the pending command's missing authority set.
/// Currently execute_request_permissions calls consume before the coverage check → red.
#[cfg(unix)]
#[tokio::test]
async fn consume_pending_once_not_called_when_grant_does_not_cover_missing_set() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::Cell;
    use std::rc::Rc;

    let consumed = Rc::new(Cell::new(false));

    struct ConsumeSpy {
        consumed: Rc<Cell<bool>>,
        base: Caveats,
    }
    impl PermissionGate for ConsumeSpy {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, _kind: DenialKind, _target: &str) {
            self.consumed.set(true);
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let other_path = ws.path().join("other.txt").to_string_lossy().into_owned();
    let out_path = ws.path().join("out.txt").to_string_lossy().into_owned();

    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    // The pending command needs FsWrite(other_path) to replay.
    let missing_req = PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::FsWrite,
        target: other_path.clone(),
        reason: "test".to_string(),
        harness_bound: false,
    };
    // The operator approves FsWrite(out_path) — a DIFFERENT target than missing_req.
    let mut pending_slot: Option<super::super::PendingRerun> = Some(super::super::PendingRerun {
        cmd: format!("/bin/touch {other_path}"),
        cwd: workspace_str.clone(),
        declared: vec![missing_req.clone()],
        missing: vec![missing_req],
    });

    let mut spy = ConsumeSpy {
        consumed: consumed.clone(),
        base: base.clone(),
    };
    let _ = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({
            "capability": "fs_write",
            "target": out_path,
            "reason": "test grant",
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut spy as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_slot),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        !consumed.get(),
        "#2636 finding 3: consume_pending_once must not fire when the approved grant \
         does not cover the pending missing set (FsWrite({out_path}) ≠ FsWrite({other_path}))"
    );
}

/// #2636 round3 blocker 1: the "IMMEDIATELY re-runs" bound wording must only
/// fire when the requested grant covers the COMPLETE `missing` set a pending
/// command was denied on. Drives `execute_tool_with_collaborators` through a
/// capturing spy gate and inspects the `PermissionRequest` the operator would
/// see (`harness_bound`, wording). Does not check command execution or
/// once-grant lifetime — those are covered by
/// `stateful_gate_proves_ineligible_no_replay_and_eligible_once()` (round4).
#[cfg(unix)]
#[tokio::test]
async fn bound_wording_and_replay_require_complete_missing_coverage() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::RefCell;
    use std::rc::Rc;

    struct CaptureGate {
        base: Caveats,
        captured: Rc<RefCell<Option<PermissionRequest>>>,
    }
    impl PermissionGate for CaptureGate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            *self.captured.borrow_mut() = requests.first().cloned();
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, _kind: DenialKind, _target: &str) {}
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let path_a = ws.path().join("a.txt").to_string_lossy().into_owned();
    let path_b = ws.path().join("b.txt").to_string_lossy().into_owned();
    let unrelated = ws
        .path()
        .join("unrelated.txt")
        .to_string_lossy()
        .into_owned();

    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        fs_read: crate::caveats::Scope::none(),
        ..Caveats::top()
    };

    let missing_two: Vec<PermissionRequest> = vec![
        PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::FsWrite,
            target: path_a.clone(),
            reason: "test".to_string(),
            harness_bound: false,
        },
        PermissionRequest {
            tool: "run_command".to_string(),
            kind: DenialKind::FsWrite,
            target: path_b.clone(),
            reason: "test".to_string(),
            harness_bound: false,
        },
    ];

    // capability/target for the operator's grant, and whether the wording
    // should promise an immediate replay.
    let cases: Vec<(&str, String, bool)> = vec![
        ("fs_write", unrelated.clone(), false), // unrelated target
        ("fs_read", path_a.clone(), false),     // wrong axis
        ("fs_write", path_a.clone(), false),    // partial set (only path_a of two)
        // complete set needs one grant that covers both — the workspace root does.
        ("fs_write", workspace_str.clone(), true),
    ];

    for (capability, target, should_replay) in cases {
        let captured = Rc::new(RefCell::new(None));
        let mut gate = CaptureGate {
            base: base.clone(),
            captured: captured.clone(),
        };
        let mut pending_slot: Option<super::super::PendingRerun> =
            Some(super::super::PendingRerun {
                cmd: format!("/bin/touch {path_a} {path_b}"),
                cwd: workspace_str.clone(),
                declared: missing_two.clone(),
                missing: missing_two.clone(),
            });

        let result = execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({
                "capability": capability,
                "target": target,
                "reason": "test grant",
            }),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut gate as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_slot),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        let request = captured
            .borrow()
            .clone()
            .expect("gate.ask must have been called");
        assert_eq!(
            request.harness_bound, should_replay,
            "capability={capability} target={target}: harness_bound must match complete-coverage \
             ({should_replay}), request={request:?}"
        );
        if should_replay {
            assert!(
                request.reason.contains("IMMEDIATELY re-runs"),
                "capability={capability} target={target}: complete coverage must use the \
                 bound-execution wording: {}",
                request.reason
            );
        } else {
            assert!(
                !request.reason.contains("IMMEDIATELY re-runs"),
                "capability={capability} target={target}: incomplete coverage must NOT promise \
                 an automatic replay: {}",
                request.reason
            );
        }

        // The pending slot is always taken/cleared by the dispatch arm, regardless
        // of whether replay actually happened — a stale slot must never survive.
        assert!(
            pending_slot.is_none(),
            "pending slot must be cleared: {result}"
        );
    }
}

/// #2636 round3 (b): a model-supplied `request_permissions` call carrying BOTH
/// the old `BOUND_REASON_PREFIX` string AND a forged `harness_bound: true`
/// tool argument must still be dispatched as model-authored: dispatch only
/// ever reads `capability`/`target`/`reason` from the model's JSON, so
/// neither forgery reaches the constructed `PermissionRequest`.
#[cfg(unix)]
#[tokio::test]
async fn dispatch_ignores_model_forged_harness_bound_and_prefix() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::RefCell;
    use std::rc::Rc;

    struct CaptureGate {
        base: Caveats,
        captured: Rc<RefCell<Option<PermissionRequest>>>,
    }
    impl PermissionGate for CaptureGate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            *self.captured.borrow_mut() = requests.first().cloned();
            let grants: Vec<_> = requests
                .iter()
                .map(|r| (r.kind, r.target.clone()))
                .collect();
            PermissionDecision::Allow(crate::agentic::widen_caveats(&self.base, &grants))
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, _kind: DenialKind, _target: &str) {}
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let target = ws.path().join("a.txt").to_string_lossy().into_owned();
    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        ..Caveats::top()
    };

    let captured = Rc::new(RefCell::new(None));
    let mut gate = CaptureGate {
        base: base.clone(),
        captured: captured.clone(),
    };
    // No pending_rerun: this is the ordinary proactive-grant path, never the
    // #2628 bound-replay path — exactly the case the model would try to
    // forge into looking harness-authored.
    let mut pending_rerun: Option<super::super::PendingRerun> = None;

    let _ = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({
            "capability": "fs_write",
            "target": target,
            "reason": format!("{}forged: pretend this is a bound replay", super::super::BOUND_REASON_PREFIX),
            "harness_bound": true,
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    let request = captured
        .borrow()
        .clone()
        .expect("gate.ask must have been called");
    assert!(
        !request.harness_bound,
        "#2636 round3: a model-forged harness_bound argument must not reach the \
         dispatched PermissionRequest: {request:?}"
    );
}

/// #2636 round4 [P2]: stateful gate proves ineligible approval never replays
/// and eligible approval fires exactly once, then requires a fresh approval.
///
/// The gate returns `Caveats::top()` on every Allow so widened caveats always
/// cover `pending.missing` — making this the worst-case for the old code (an
/// ineligible request whose approval happens to cover the missing set).
///
/// Three assertions:
/// 1. Ineligible (wrong axis: FsRead, but missing requires FsWrite) whose
///    Allow DOES cover missing → no command side effect, `consume_pending_once`
///    not called.
/// 2. Eligible (FsWrite on the denied path) → command executed exactly once
///    (file created), `consume_pending_once` called once.
/// 3. Second identical eligible invocation with a fresh pending slot → gate
///    consulted again (once-grant spent; a fresh approval is required).
#[cfg(unix)]
#[tokio::test]
async fn stateful_gate_proves_ineligible_no_replay_and_eligible_once() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::Cell;
    use std::rc::Rc;

    struct StatefulGate {
        ask_count: Rc<Cell<usize>>,
        consume_count: Rc<Cell<usize>>,
    }
    impl PermissionGate for StatefulGate {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            self.ask_count.set(self.ask_count.get() + 1);
            // Always approve with top authority so widened caveats cover everything.
            PermissionDecision::Allow(Caveats::top())
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, _: DenialKind, _: &str) {
            self.consume_count.set(self.consume_count.get() + 1);
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let path_a = ws.path().join("a.txt").to_string_lossy().into_owned();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        fs_read: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let missing = vec![PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::FsWrite,
        target: path_a.clone(),
        reason: "test".to_string(),
        harness_bound: false,
    }];
    let ask_count = Rc::new(Cell::new(0usize));
    let consume_count = Rc::new(Cell::new(0usize));

    // ── CASE 1: ineligible request (FsRead axis, missing is FsWrite) ──────────
    // The gate returns top() which covers FsWrite(path_a), but the pre-prompt
    // eligibility check fails (axis mismatch) → replay_auth=None → no replay.
    {
        let mut gate = StatefulGate {
            ask_count: ask_count.clone(),
            consume_count: consume_count.clone(),
        };
        let mut pending_slot: Option<super::super::PendingRerun> =
            Some(super::super::PendingRerun {
                cmd: format!("/bin/touch {path_a}"),
                cwd: workspace_str.clone(),
                declared: missing.clone(),
                missing: missing.clone(),
            });
        execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({"capability": "fs_read", "target": &path_a, "reason": "test"}),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut gate as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_slot),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    }
    assert!(
        !std::path::Path::new(&path_a).exists(),
        "#2636 round4: ineligible approval (wrong axis) must not execute the command"
    );
    assert_eq!(
        ask_count.get(),
        1,
        "#2636 round4: gate.ask must be consulted even for an ineligible request"
    );
    assert_eq!(
        consume_count.get(),
        0,
        "#2636 round4: consume_pending_once must not fire when the request is ineligible"
    );

    // ── CASE 2a: eligible approval → exactly one execution ────────────────────
    ask_count.set(0);
    consume_count.set(0);
    {
        let mut gate = StatefulGate {
            ask_count: ask_count.clone(),
            consume_count: consume_count.clone(),
        };
        let mut pending_slot: Option<super::super::PendingRerun> =
            Some(super::super::PendingRerun {
                cmd: format!("/bin/touch {path_a}"),
                cwd: workspace_str.clone(),
                declared: missing.clone(),
                missing: missing.clone(),
            });
        execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({"capability": "fs_write", "target": &path_a, "reason": "test"}),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut gate as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_slot),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    }
    assert!(
        std::path::Path::new(&path_a).exists(),
        "#2636 round4: eligible approval must execute the command (side effect: file created)"
    );
    assert_eq!(
        consume_count.get(),
        1,
        "#2636 round4: consume_pending_once must fire exactly once for an eligible approval"
    );

    // ── CASE 2b: second identical invocation → gate consulted again ───────────
    // The once-grant was signalled spent in case 2a (consume_pending_once fired).
    // A second request_permissions with a fresh pending slot must still consult
    // the gate — no auto-approval from a cached grant.
    ask_count.set(0);
    consume_count.set(0);
    std::fs::remove_file(&path_a).unwrap();
    {
        let mut gate = StatefulGate {
            ask_count: ask_count.clone(),
            consume_count: consume_count.clone(),
        };
        let mut pending_slot: Option<super::super::PendingRerun> =
            Some(super::super::PendingRerun {
                cmd: format!("/bin/touch {path_a}"),
                cwd: workspace_str.clone(),
                declared: missing.clone(),
                missing: missing.clone(),
            });
        execute_tool_with_collaborators(
            "request_permissions",
            &serde_json::json!({"capability": "fs_write", "target": &path_a, "reason": "test"}),
            &workspace_str,
            false,
            20,
            &base,
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: Some(&mut gate as &mut dyn PermissionGate),
                pending_rerun: Some(&mut pending_slot),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    }
    assert_eq!(
        ask_count.get(),
        1,
        "#2636 round4: second eligible invocation must consult gate for a fresh approval \
         (once-grant spent after case 2a)"
    );
}

/// #2636 round5 [P2]: a REAL queue-owning gate (mirrors the TUI gate's
/// `pending_once_grants`: `ask` for a non-`request_permissions` tool checks
/// the queue FIRST and auto-approves without consulting the operator when a
/// matching grant is queued; `request_permissions` always queues a proactive
/// once-grant on `Allow`; `consume_pending_once` removes it). Round4's
/// "stateful" gate only counted asks — it never modeled the queue a leftover
/// grant lives in, so it could not tell an operator prompt apart from an
/// auto-grant, nor prove a spent grant stays spent.
///
/// One gate instance owns the queue across the whole sequence:
/// 1. Proactive grant with no #2628 binding queues a once-grant; the model's
///    own plain `run_command` retry then auto-approves from the queue —
///    ZERO operator prompts for that second call, proving the gate can
///    auto-grant without consulting the operator (`prompt_count` stays flat
///    while `ask_count` rises).
/// 2. A #2628-bound approval (eligible, covers `pending.missing`) replays
///    the denied command exactly once — proven by a counting side effect
///    (line count, not mere existence) — and `consume_pending_once` removes
///    the queued grant it just spent.
/// 3. An ordinary matching `run_command` run afterward must consult the
///    operator AGAIN (a fresh prompt) rather than reusing the leftover
///    grant consumed in step 2.
#[cfg(unix)]
#[tokio::test]
async fn queue_owning_gate_proves_once_grant_lifetime_across_replay_and_fresh_command() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::rc::Rc;

    struct QueueGate {
        queue: Rc<RefCell<BTreeSet<(DenialKind, String)>>>,
        ask_count: Rc<Cell<usize>>,
        prompt_count: Rc<Cell<usize>>,
        consume_count: Rc<Cell<usize>>,
    }
    impl PermissionGate for QueueGate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.ask_count.set(self.ask_count.get() + 1);
            let req = &requests[0];
            if req.tool != "request_permissions" {
                let key = (req.kind, req.target.clone());
                if self.queue.borrow_mut().remove(&key) {
                    // Auto-approved from the queued once-grant: the operator
                    // is never consulted for this call.
                    return PermissionDecision::Allow(Caveats::top());
                }
            }
            self.prompt_count.set(self.prompt_count.get() + 1);
            if req.tool == "request_permissions" {
                self.queue
                    .borrow_mut()
                    .insert((req.kind, req.target.clone()));
            }
            PermissionDecision::Allow(Caveats::top())
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, kind: DenialKind, target: &str) {
            self.consume_count.set(self.consume_count.get() + 1);
            self.queue.borrow_mut().remove(&(kind, target.to_string()));
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let hits = ws.path().join("hits");
    std::fs::create_dir(&hits).unwrap();
    // The grant target is the `hits` DIRECTORY, not the file each `mkdir`
    // creates inside it: `permits_path` matches by prefix, so a grant on
    // `hits` covers every `hits/N` the real (Landlock-backed) sandbox
    // actually enforces, while the queue-matching key (an exact
    // `(kind, target)` pair) still lines up between the proactive grant and
    // the model's retry.
    let hits_str = hits.to_string_lossy().into_owned();
    // fs_read stays at `top()` (workspace browsing is ordinary authority);
    // only fs_write is denied so every `mkdir` needs a grant. Denying
    // fs_read too would make `ask_with_caveats`'s default `.meet(widen_caveats(
    // baseline, grants))` narrow the CWD read scope for the plain
    // `run_command` path (unlike `request_permissions`'s direct `gate.ask`,
    // which returns the gate's `Allow` caveats unmet) and every ordinary
    // `run_command` call would fail before it ever reached `mkdir`.
    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    // `mkdir` (unlike `touch`) fails loudly on a second attempt against the
    // same path, so a hit count is genuine proof of exactly-once execution,
    // not merely at-least-once — the gap the round4 review flagged.
    let hit_count = || std::fs::read_dir(&hits).unwrap().count();

    let queue = Rc::new(RefCell::new(BTreeSet::new()));
    let ask_count = Rc::new(Cell::new(0usize));
    let prompt_count = Rc::new(Cell::new(0usize));
    let consume_count = Rc::new(Cell::new(0usize));
    let mut gate = QueueGate {
        queue: queue.clone(),
        ask_count: ask_count.clone(),
        prompt_count: prompt_count.clone(),
        consume_count: consume_count.clone(),
    };

    // ── STEP 1: proactive grant (no #2628 binding) queues a once-grant ────────
    let mut no_pending: Option<super::super::PendingRerun> = None;
    execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({"capability": "fs_write", "target": &hits_str, "reason": "test"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut no_pending),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prompt_count.get(),
        1,
        "proactive grant must prompt the operator once"
    );
    assert!(
        queue
            .borrow()
            .contains(&(DenialKind::FsWrite, hits_str.clone())),
        "AllowOnce on request_permissions must queue a proactive once-grant"
    );

    // The model's OWN retry: an ordinary matching run_command auto-approves
    // from the queue — the gate is consulted (`ask_count` rises) but the
    // operator is NOT (`prompt_count` stays flat).
    let hit1 = hits.join("1").to_string_lossy().into_owned();
    execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/mkdir {hit1}"),
            "fs_write": [&hits_str],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut no_pending),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prompt_count.get(),
        1,
        "the model's own retry must auto-grant from the queued once-grant, \
         with NO fresh operator prompt"
    );
    assert_eq!(hit_count(), 1, "retry must run the command exactly once");
    assert!(
        queue.borrow().is_empty(),
        "the once-grant must be consumed by the retry that spent it"
    );

    // ── STEP 2: #2628-bound approval replays exactly once, then consumes ──────
    let missing = vec![PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::FsWrite,
        target: hits_str.clone(),
        reason: "test".to_string(),
        harness_bound: false,
    }];
    let hit2 = hits.join("2").to_string_lossy().into_owned();
    let mut pending_slot: Option<super::super::PendingRerun> = Some(super::super::PendingRerun {
        cmd: format!("/bin/mkdir {hit2}"),
        cwd: workspace_str.clone(),
        declared: missing.clone(),
        missing: missing.clone(),
    });
    execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({"capability": "fs_write", "target": &hits_str, "reason": "test"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_slot),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prompt_count.get(),
        2,
        "#2628 approval must prompt the operator"
    );
    assert_eq!(
        consume_count.get(),
        1,
        "eligible+covering approval must consume the once-grant"
    );
    assert_eq!(
        hit_count(),
        2,
        "the replay must run the bound command exactly once (2 = 1 retry + 1 replay)"
    );
    assert!(
        queue.borrow().is_empty(),
        "consume_pending_once must leave nothing behind for a later command to reuse"
    );

    // ── STEP 3: a later ordinary matching run_command requires a FRESH prompt ─
    let hit3 = hits.join("3").to_string_lossy().into_owned();
    execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!("/bin/mkdir {hit3}"),
            "fs_write": [&hits_str],
        }),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut no_pending),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prompt_count.get(),
        3,
        "#2636 round5: a matching run_command after the #2628 replay must NOT reuse the \
         consumed grant — it must require a fresh operator approval"
    );
    assert_eq!(
        hit_count(),
        3,
        "the fresh-approval command must also run exactly once"
    );
}

/// #2636 round5 [P2]: an approval that is ELIGIBLE pre-prompt (the requested
/// capability/target alone would, on paper, cover the complete `missing`
/// set) but whose RETURNED `widened` caveats do not actually cover it (the
/// gate granted something narrower/different than what it was asked, e.g. a
/// danger-tier fence or a stale caveats computation) must neither replay the
/// command NOR consume the queued once-grant. Consuming here would silently
/// destroy authority the operator never got to spend on anything, while
/// telling the model nothing ran.
#[cfg(unix)]
#[tokio::test]
async fn eligible_pre_prompt_but_insufficient_allow_neither_executes_nor_consumes() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::Cell;
    use std::rc::Rc;

    struct InsufficientGate {
        consumed: Rc<Cell<bool>>,
    }
    impl PermissionGate for InsufficientGate {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            // Allow, but the returned caveats grant NOTHING for fs_write —
            // eligible pre-prompt (target matches exactly), insufficient
            // post-prompt.
            PermissionDecision::Allow(Caveats {
                fs_write: crate::caveats::Scope::none(),
                ..Caveats::top()
            })
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, _: DenialKind, _: &str) {
            self.consumed.set(true);
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace_str = ws.path().to_string_lossy().into_owned();
    let target = ws.path().join("a.txt").to_string_lossy().into_owned();
    let base = Caveats {
        fs_write: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let missing = vec![PermissionRequest {
        tool: "run_command".to_string(),
        kind: DenialKind::FsWrite,
        target: target.clone(),
        reason: "test".to_string(),
        harness_bound: false,
    }];
    let mut pending_slot: Option<super::super::PendingRerun> = Some(super::super::PendingRerun {
        cmd: format!("/bin/touch {target}"),
        cwd: workspace_str.clone(),
        declared: missing.clone(),
        missing: missing.clone(),
    });
    let consumed = Rc::new(Cell::new(false));
    let mut gate = InsufficientGate {
        consumed: consumed.clone(),
    };

    execute_tool_with_collaborators(
        "request_permissions",
        // Requesting EXACTLY the missing target — eligible pre-prompt.
        &serde_json::json!({"capability": "fs_write", "target": &target, "reason": "test"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            pending_rerun: Some(&mut pending_slot),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        !std::path::Path::new(&target).exists(),
        "#2636 round5: an eligible-but-insufficient Allow must not execute the replay"
    );
    assert!(
        !consumed.get(),
        "#2636 round5: an eligible-but-insufficient Allow must not consume the once-grant \
         either — the operator's grant was never actually spent on anything"
    );
}

/// #2681 round 2/3 (P3): a REAL queue-owning gate (mirrors the SHAPE of
/// newt-tui's `pending_once_grants`/`danger` policy — see
/// `broker_bearing_commit_denial_never_consults_the_gate`'s doc comment for
/// the queue's real shape — but this gate's own `high_danger` set and its
/// refusal are a stand-in this test writes itself, NOT a test of newt-tui's
/// actual danger policy, which lives in a different crate) proves the EXEC
/// axis's once-grant lifetime end to end, where `execute_permissions_
/// reruns_denied_run_command_for_exec` (stateless `MockGate`, always allows)
/// could not: that test stops after one successful replay, so it cannot show
/// a spent grant staying spent, an unrelated target staying ungranted, or a
/// simulated high-danger target never getting a durable grant at all.
///
/// One gate instance owns the queue across the whole sequence:
/// 1. `run_command` with a single simple `/bin/echo hello` (exec:none) —
///    denied, and (per `exec_denial_is_replay_safe`'s rule) replay-eligible:
///    exactly one inventoried command, no constructs, no warnings, no
///    mutating redirect.
/// 2. `request_permissions` approves exec for `/bin/echo` — the bound
///    replay runs the command exactly once under that ONE grant
///    (`result.contains("hello")`) — the once-grant is then consumed. (A
///    `&&`-chained command that execs the SAME target twice is no longer
///    eligible at all under round 3's rule — see
///    `exec_replay_excluded_for_compound_and_pipeline_shapes`.)
/// 3. A FRESH, separate `run_command` for the SAME target afterward is
///    DENIED — the spent grant does not carry over to a new invocation.
/// 4. Control — unrelated target: granting `/bin/echo` never queues
///    `/bin/mkdir`; a separate `run_command` for `/bin/mkdir` is denied too.
/// 5. Control — simulated high danger: `request_permissions(exec,
///    "/bin/rm")` against THIS gate's own hardcoded `high_danger` set is
///    refused outright and never reaches the queue — proven by `ask_count`
///    rising with no corresponding queue entry and no replay. This is a
///    stand-in for an operator declining a high-danger target, not a
///    regression test of newt-tui's production danger policy.
#[cfg(unix)]
#[tokio::test]
async fn queue_owning_gate_proves_exec_once_grant_lifetime_and_danger_controls() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = super::disable_ocap_tests::EnvVar::unset("NEWT_DISABLE_OCAP");

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::rc::Rc;

    struct ExecQueueGate {
        queue: Rc<RefCell<BTreeSet<(DenialKind, String)>>>,
        high_danger: BTreeSet<String>,
        ask_count: Rc<Cell<usize>>,
        consume_count: Rc<Cell<usize>>,
    }
    impl PermissionGate for ExecQueueGate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.ask_count.set(self.ask_count.get() + 1);
            let req = &requests[0];
            let key = (req.kind, req.target.clone());
            if req.tool != "request_permissions" {
                if self.queue.borrow_mut().remove(&key) {
                    // Auto-approved from the queued once-grant: no fresh
                    // operator decision for this call.
                    return PermissionDecision::Allow(Caveats::top());
                }
                // Nothing queued for this target — no grant was given.
                return PermissionDecision::Deny;
            }
            if self.high_danger.contains(&req.target) {
                // Stand-in for an operator declining a high-danger target
                // outright, mirroring the SHAPE of newt-tui's danger policy
                // (never this gate's own production logic) — it never
                // reaches the queue at all.
                return PermissionDecision::Deny;
            }
            self.queue.borrow_mut().insert(key);
            PermissionDecision::Allow(Caveats::top())
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn consume_pending_once(&mut self, kind: DenialKind, target: &str) {
            self.consume_count.set(self.consume_count.get() + 1);
            self.queue.borrow_mut().remove(&(kind, target.to_string()));
        }
    }

    let ws = tempfile::tempdir().unwrap();
    let workspace = ws.path().canonicalize().unwrap();
    let workspace_str = workspace.to_string_lossy().into_owned();
    let base = Caveats {
        exec: Scope::none(),
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..caveats_rw(&workspace)
    };

    let queue = Rc::new(RefCell::new(BTreeSet::new()));
    let ask_count = Rc::new(Cell::new(0usize));
    let consume_count = Rc::new(Cell::new(0usize));
    let mut gate = ExecQueueGate {
        queue: queue.clone(),
        high_danger: BTreeSet::from(["/bin/rm".to_string()]),
        ask_count: ask_count.clone(),
        consume_count: consume_count.clone(),
    };

    // ── STEP 1: run_command denied — /bin/echo not granted ──────────────────
    let mut pending_rerun: Option<crate::agentic::tools::PendingRerun> = None;
    let execution1 = std::sync::OnceLock::<ExecOutcome>::new();
    let result1 = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "/bin/echo hello"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            execution: Some(&execution1),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(execution1.get(), Some(&ExecOutcome::Denied), "{result1}");
    assert!(
        pending_rerun.is_some(),
        "setup: the denial must populate pending_rerun: {result1}"
    );
    assert!(
        queue.borrow().is_empty(),
        "setup: nothing is queued before any request_permissions call"
    );

    // ── STEP 2: bound approval replays the command exactly once ─────────────
    let execution2 = std::sync::OnceLock::<ExecOutcome>::new();
    let result2 = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({"capability": "exec", "target": "/bin/echo", "reason": "test"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            execution: Some(&execution2),
            pending_rerun: Some(&mut pending_rerun),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        execution2.get(),
        Some(&ExecOutcome::Passed),
        "the bound replay must actually run: {result2}"
    );
    assert!(result2.contains("hello"), "{result2}");
    assert_eq!(
        consume_count.get(),
        1,
        "the replay must consume the once-grant exactly once"
    );
    assert!(
        queue.borrow().is_empty(),
        "nothing must be left queued after the replay consumed it"
    );
    assert!(pending_rerun.is_none());

    // ── STEP 3: a FRESH, separate run_command for the SAME target is denied ─
    let execution3 = std::sync::OnceLock::<ExecOutcome>::new();
    let _ = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "/bin/echo three"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            execution: Some(&execution3),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        execution3.get(),
        Some(&ExecOutcome::Denied),
        "#2681 round 2: the spent once-grant must NOT carry over to a fresh, \
         separate invocation of the same target"
    );

    // ── Control: an unrelated target never got queued, and stays denied ────
    let execution4 = std::sync::OnceLock::<ExecOutcome>::new();
    let _ = execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({"command": "/bin/mkdir /nonexistent-control-path"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            execution: Some(&execution4),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        execution4.get(),
        Some(&ExecOutcome::Denied),
        "control: granting /bin/echo must never leak exec authority to an \
         unrelated target"
    );

    // ── Control: a high-danger target never gets a durable/queued grant ────
    let asks_before = ask_count.get();
    let execution5 = std::sync::OnceLock::<ExecOutcome>::new();
    let result5 = execute_tool_with_collaborators(
        "request_permissions",
        &serde_json::json!({"capability": "exec", "target": "/bin/rm", "reason": "test"}),
        &workspace_str,
        false,
        20,
        &base,
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: Some(&mut gate as &mut dyn PermissionGate),
            execution: Some(&execution5),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        ask_count.get() > asks_before,
        "control: the gate must actually have been consulted for the \
         high-danger target"
    );
    assert!(
        result5.contains("denied"),
        "control: a high-danger exec target must be refused outright: {result5}"
    );
    assert!(
        !queue
            .borrow()
            .contains(&(DenialKind::Exec, "/bin/rm".to_string())),
        "control: a refused high-danger request must never reach the queue"
    );
    assert_ne!(execution5.get(), Some(&ExecOutcome::Passed));
}
