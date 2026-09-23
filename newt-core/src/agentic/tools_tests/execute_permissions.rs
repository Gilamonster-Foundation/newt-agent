use super::*;
use crate::ExecOutcome;

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
    );
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
    );
    assert!(out.starts_with("denied:"), "got: {out}");
    assert!(out.contains("different approach"), "got: {out}");

    // NO gate (headless / eval) → "no operator available" — recoverable,
    // never a hang or a config-only dead end.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "net", "target": "docs.rs", "reason": "fetch"}),
        None,
        false,
        20,
    );
    assert!(out.contains("no operator available"), "got: {out}");
}

#[test]
fn permission_grant_releases_only_cached_authority_failures() {
    use crate::agentic::RepeatCallGuard;

    let args = serde_json::json!({"path": "report.txt"});
    let permission = serde_json::json!({"capability": "fs_read", "target": "report.txt"});
    for result_aware in [false, true] {
        let mut guard = RepeatCallGuard::for_verification(result_aware);
        guard.record(
            "read_file",
            &args,
            false,
            &denied_fs_result("fs_read", "report.txt"),
            None,
        );
        guard.record("list_dir", &args, false, "error: not a directory", None);
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
        );
        let os_error = serde_json::json!({"command": "check permissions"});
        guard.record(
            "run_command",
            &os_error,
            false,
            "error: fixture asserted Permission denied",
            None,
        );
        guard.record("state_get", &args, true, "no such key: report.txt", None);
        let fetch = serde_json::json!({"url": "https://example.test/report"});
        guard.record("web_fetch", &fetch, true, "observed report", None);
        assert!(guard.repeat_steer("read_file", &args).is_some());

        let mut allow = MockGate::new(true, &Caveats::top());
        let mut deny = MockGate::new(false, &Caveats::top());
        let granted = execute_request_permissions(&permission, Some(&mut allow), false, 20);
        for (name, request, result) in [
            (
                "request_permissions",
                permission.clone(),
                execute_request_permissions(&permission, Some(&mut deny), false, 20),
            ),
            (
                "request_permissions",
                permission.clone(),
                execute_request_permissions(&permission, None, false, 20),
            ),
            (
                "request_permissions",
                serde_json::json!({}),
                execute_request_permissions(&serde_json::json!({}), Some(&mut allow), false, 20),
            ),
            ("read_file", args.clone(), granted.clone()),
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
        ] {
            guard.record(name, &request, tool_result_ok(&result), &result, None);
            assert!(
                guard.repeat_steer("read_file", &args).is_some(),
                "{name}: {result}"
            );
        }

        guard.record(
            "request_permissions",
            &permission,
            tool_result_ok(&granted),
            &granted,
            None,
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
            4,
            "invalidation does not erase executed history"
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
    for result_aware in [false, true] {
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
                let mut guard = RepeatCallGuard::for_verification(result_aware);
                guard.record(name, &command, tool_result_ok(failed), failed, outcome);
                guard.record(
                    "read_file",
                    &unrelated,
                    false,
                    "error: not a directory",
                    None,
                );
                assert!(guard.repeat_steer(name, &command).is_some());
                let declined = execute_request_permissions(&permission, None, false, 20);
                guard.record(
                    "request_permissions",
                    &permission,
                    tool_result_ok(&declined),
                    &declined,
                    None,
                );
                assert!(guard.repeat_steer(name, &command).is_some());

                let mut gate = MockGate::new(true, &Caveats::top());
                let granted = execute_request_permissions(&permission, Some(&mut gate), false, 20);
                guard.record(
                    "request_permissions",
                    &permission,
                    tool_result_ok(&granted),
                    &granted,
                    None,
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
                guard.record(name, &command, tool_result_ok(failed), failed, outcome);
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
    guard.record("list_dir", &args, tool_result_ok(&denied), &denied, None);
    assert!(guard.repeat_steer("list_dir", &args).is_some());

    let mut gate = MockGate::new(true, &base);
    let permission = serde_json::json!({"capability": "fs_read", "target": file});
    let granted = execute_request_permissions(&permission, Some(&mut gate), false, 20);
    assert!(granted.starts_with("granted:"), "{granted}");
    guard.record(
        "request_permissions",
        &permission,
        tool_result_ok(&granted),
        &granted,
        None,
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
    guard.record("list_dir", &args, tool_result_ok(&retried), &retried, None);
    guard.record(
        "request_permissions",
        &permission,
        tool_result_ok(&granted),
        &granted,
        None,
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
    );
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
    );
    assert!(out.contains("unknown capability"), "got: {out}");
    assert!(out.contains("fs_read"), "got: {out}");
    // Missing target → coach.
    let out = execute_request_permissions(
        &serde_json::json!({"capability": "exec", "reason": "y"}),
        None,
        false,
        20,
    );
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
    assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
    let out = run_tool(
        "list_dir",
        serde_json::json!({"path": "."}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_read", "."));
    let out = run_tool(
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "c"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
    let out = run_tool(
        "edit_file",
        serde_json::json!({"path": "a.txt", "old_string": "a", "new_string": "b"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "a.txt"));
    let out = run_tool(
        "delete_file",
        serde_json::json!({"path": "secret.txt"}),
        ws.path(),
        &denied,
        None,
    )
    .await;
    assert_eq!(out, denied_fs_result("fs_write", "secret.txt"));
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
                let result = execute_request_permissions(&args, Some(&mut gate), false, 20);
                assert!(permission_grant_succeeded(
                    "request_permissions",
                    &args,
                    true,
                    &result
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
    assert_eq!(gated, denied_fs_result("fs_read", "secret.txt"));
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
    assert_eq!(out, denied_fs_result("fs_read", "secret.txt"));
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
