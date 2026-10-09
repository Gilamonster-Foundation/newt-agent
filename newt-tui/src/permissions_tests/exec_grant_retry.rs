//! A real shell dispatch must spend the operator's queued interpreter grant.
use super::*;

async fn dispatch(
    command: &str,
    root: &std::path::Path,
    base: &Caveats,
    gate: &mut dyn newt_core::PermissionGate,
) -> String {
    newt_core::execute_tool(
        "run_command",
        &serde_json::json!({"command": command}),
        &root.to_string_lossy(),
        false,
        20,
        base,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None,
        Some(gate),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

/// Regression: a compound bash/sh script invocation reports a held allow-once
/// grant, then its identical retry must execute rather than requeue that grant.
/// Grounds the production TUI danger policy and pending queue in kernel-fenced
/// dispatch on Linux and macOS; no mocked gate and no kernel skip.
#[tokio::test]
async fn interpreter_allow_once_executes_the_matching_retry() {
    use crate::disable_ocap_session_tests::EnvVar;
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = EnvVar::unset("NEWT_FULL_ACCESS");
    assert!(newt_core::confined_exec::kernel_fs_fence_available());
    for (program, shape) in ["bash", "sh", "/bin/bash", "/bin/sh"]
        .into_iter()
        .flat_map(|program| (0..3).map(move |shape| (program, shape)))
    {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::write(root.join("script.sh"), "printf 'ran\n' >> marker\n").unwrap();
        let marker = root.join("marker");
        let baseline = Caveats {
            exec: Scope::none(),
            #[cfg(target_os = "macos")]
            net: Scope::All,
            ..newt_core::confined_exec::workspace_confined_caveats(&root)
        };
        let mut state = PermissionPromptState::default();
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            baseline.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce, PromptChoice::Deny],
            prompts.clone(),
        );
        let command = match shape {
            0 => format!("{program} script.sh"),
            1 => format!("cd . && {program} script.sh 2>&1"),
            _ => format!("{program} script.sh; {program} -c true"),
        };
        let first = dispatch(&command, &root, &baseline, &mut gate).await;
        assert!(first.starts_with("granted:"), "{program}: {first}");
        assert!(
            !marker.exists(),
            "approval must not auto-replay a compound command"
        );
        assert_eq!(prompts.get(), 1);
        let queued = gate.pending_command_retries.clone();
        assert_eq!(queued.len(), 1, "{first}");
        let retry = dispatch(&command, &root, &baseline, &mut gate).await;
        assert_eq!(
            std::fs::read_to_string(&marker).ok().as_deref(),
            Some("ran\n"),
            "{program}: queued={queued:?}, retry={retry}"
        );
        assert_eq!(prompts.get(), 1, "retry uses the existing once grant");
        assert!(gate.pending_command_retries.is_empty());
        assert!(gate.state.pending_once_grants.is_empty());
        assert!(gate.state.session_grants.is_empty());
        let refused = dispatch(&command, &root, &baseline, &mut gate).await;
        assert_eq!(
            prompts.get(),
            2,
            "fresh invocation must ask again: {refused}"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
    }
}

/// Exact invocation binding must not become a basename, cwd, or session grant.
#[test]
fn command_retry_is_exact_one_shot_and_respects_denial_and_clamps() {
    let mut state = PermissionPromptState::default();
    let base = base_caveats("/ws");
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![],
        Rc::new(Cell::new(0)),
    );
    let request = exec_request("/bin/bash");
    let command = "bash script.sh";
    gate.queue_command_retry(command, "/ws", std::slice::from_ref(&request));
    for (cmd, cwd) in [
        ("bash other.sh", "/ws"),
        (command, "/other"),
        ("/other/bash script.sh", "/ws"),
    ] {
        assert_eq!(gate.apply_command_retry(cmd, cwd, &base), base);
        assert_eq!(gate.pending_command_retries.len(), 1);
    }
    gate.preset_clamp = Some(base.clone());
    assert_eq!(gate.apply_command_retry(command, "/ws", &base), base);
    assert_eq!(
        gate.pending_command_retries.len(),
        1,
        "insufficient authority cannot spend the grant"
    );
    gate.preset_clamp = None;
    let granted = gate.apply_command_retry(command, "/ws", &base);
    assert!(granted.permits_exec("/bin/bash"));
    assert!(!granted.permits_exec("/other/bash"));
    assert_eq!(granted.fs_write, base.fs_write);
    assert_eq!(granted.net, base.net);
    assert_eq!(gate.apply_command_retry(command, "/ws", &base), base);
    assert!(gate.state.session_grants.is_empty());
    gate.queue_command_retry(command, "/ws", &[request]);
    gate.state
        .session_denials
        .insert((DenialKind::Exec, "/bin/bash".into()));
    assert_eq!(gate.apply_command_retry(command, "/ws", &base), base);
    assert!(gate.pending_command_retries.is_empty());
}

/// #2823: an unused interpreter approval must not survive the host turn's gate.
#[tokio::test]
async fn unused_approval_expires_when_the_turn_gate_is_dropped() {
    use crate::disable_ocap_session_tests::EnvVar;
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = EnvVar::unset("NEWT_FULL_ACCESS");
    assert!(newt_core::confined_exec::kernel_fs_fence_available());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("script.sh"), "printf ran >> marker\n").unwrap();
    let base = Caveats {
        exec: Scope::none(),
        #[cfg(target_os = "macos")]
        net: Scope::All,
        ..newt_core::confined_exec::workspace_confined_caveats(&root)
    };
    let mut state = PermissionPromptState::default();
    let prompts = Rc::new(Cell::new(0));
    {
        let mut gate = scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            prompts.clone(),
        );
        assert!(dispatch("bash script.sh", &root, &base, &mut gate)
            .await
            .starts_with("granted:"));
        assert_eq!(
            gate.apply_command_retry("bash unrelated.sh", root.to_str().unwrap(), &base),
            base
        );
        assert!(!root.join("marker").exists());
    }
    let mut next_turn = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::Deny],
        prompts.clone(),
    );
    next_turn.conversation_id = "another-conversation".into();
    let result = dispatch("bash script.sh", &root, &base, &mut next_turn).await;
    assert_eq!(
        prompts.get(),
        2,
        "unused authority survived turn/conversation boundary: {result}"
    );
    assert!(!root.join("marker").exists());
}
