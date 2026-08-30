use super::*;
#[cfg(unix)]
use newt_core::agentic::execute_tool;
#[cfg(unix)]
use newt_core::caveats::{Caveats, CountBound, Scope};

/// #774 (P0) — PURE: the operator's `[tui.permissions]` exec clamp is a
/// NON-OPTIONAL floor, sourced into `exec_floor` even with NO active
/// `/posture`. This is the red→green regression for design-review F1: before
/// #774 the floor was sourced from the active posture alone, so a configured
/// clamp yielded `exec_floor == None` without one, and an out-of-clamp
/// command took the `--disable-ocap` bypass unconfined.
#[test]
fn tui_permissions_exec_clamp_is_an_always_on_floor_without_posture() {
    use newt_core::caveats::{Scope, ScopeExt as _};
    // `[tui.permissions]` configures a restrictive exec clamp; no posture.
    let configured_exec: Scope<String> = Scope::only(["cargo".to_string(), "git".to_string()]);
    let floor = exec_floor_from(&configured_exec, /* posture_active = */ false).expect(
        "a configured [tui.permissions] exec clamp must be an always-on floor \
             even without a /posture — on the pre-#774 code this was None, so an \
             out-of-clamp command ran unconfined under --disable-ocap",
    );
    // An out-of-clamp command is NOT authorized by the floor → it can never
    // take the unconfined bypass; it falls through to the confined shell.
    assert!(
        !floor.permits(&"rm".to_string()),
        "an out-of-clamp command must be denied by the always-on floor"
    );
    // The configured commands stay authorized.
    assert!(floor.permits(&"cargo".to_string()));
    assert!(floor.permits(&"git".to_string()));
}

/// #774 (P0) — PURE: the floor only NARROWS (OCAP meet-only). `None` is
/// returned ONLY when exec is unrestricted (`Scope::All`) AND no posture
/// permission floor is active, leaving the unrestricted `--disable-ocap`
/// bypass exactly as it was pre-#307; any restriction OR configured posture
/// preset yields a floor.
#[test]
fn exec_floor_none_only_when_unrestricted_and_no_posture_floor() {
    use newt_core::caveats::Scope;
    // Unrestricted base + no posture preset ⇒ no floor.
    assert!(exec_floor_from(&Scope::<String>::All, false).is_none());
    // Unrestricted base + configured posture preset ⇒ floor present.
    assert!(exec_floor_from(&Scope::<String>::All, true).is_some());
    // Restrictive base + configured posture preset ⇒ floor present.
    assert!(exec_floor_from(&Scope::only(["git".to_string()]), true).is_some());
}

/// RAII env override (the run_command bypass, `ocap_disabled`, and
/// `full_access_requested` read the process env): restore the previous
/// value on drop, including on a failed assertion, so yolo/full-access
/// never leaks into a neighboring test. Used only under the exclusive
/// env write guard (`env_write_guard` / `env_write_guard_async`).
struct EnvVar {
    key: &'static str,
    saved: Option<String>,
}

impl EnvVar {
    fn set(key: &'static str, value: &str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, saved }
    }

    fn unset(key: &'static str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, saved }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// The banner is unmissable and names the mechanism: the issue's text,
/// the flag, and the host-shell consequence.
#[test]
fn banner_names_the_flag_and_the_consequence() {
    let banner = ocap_disabled_banner();
    assert!(banner.contains("⚠ ocap DISABLED"), "got: {banner}");
    assert!(banner.contains("--disable-ocap"), "got: {banner}");
    assert!(
        banner.contains("permitted commands may run unconfined"),
        "got: {banner}"
    );
    assert!(
        banner.contains("active exec floors can force confinement or denial"),
        "got: {banner}"
    );
}

/// The session record carries the issue's shape — `decision:
/// "ocap-disabled"`, `scope: "session"` — and lands in the same #263
/// jsonl log as prompted decisions, one line, lossless round-trip.
#[serial_test::serial(real_fs)]
#[test]
fn ocap_disabled_record_is_the_issue_shape_and_appends() {
    let rec = ocap_disabled_record("conv-297");
    assert_eq!(rec.conversation_id, "conv-297");
    assert_eq!(rec.tool, "run_command");
    assert_eq!(rec.kind, "exec");
    assert_eq!(rec.target, "*");
    assert_eq!(rec.decision, "ocap-disabled");
    assert_eq!(rec.scope, "session");

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("permission-log.jsonl");
    rec.append_jsonl(&path).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1);
    let parsed: newt_core::PermissionRecord = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed, rec);
}

/// `--full-access`: the banner is unmissable and names the mechanism —
/// the flag, the consequence, and how to get the configured preset back.
#[test]
fn full_access_banner_names_the_flag_and_the_consequence() {
    let banner = full_access_banner();
    assert!(banner.contains("⚠ FULL ACCESS"), "got: {banner}");
    assert!(banner.contains("--full-access"), "got: {banner}");
    // #926: the prose frames it as ambient authority + OCAP attenuation.
    assert!(banner.contains("full AMBIENT authority"), "got: {banner}");
    assert!(
        banner.contains("Object-Capability authority restrictions"),
        "got: {banner}"
    );
}

/// The `full-access` session record mirrors the ocap-disabled one —
/// `decision: "full-access"`, `scope: "session"` — and lands in the same
/// #263 jsonl log as prompted decisions, one line, lossless round-trip.
#[serial_test::serial(real_fs)]
#[test]
fn full_access_record_is_the_session_shape_and_appends() {
    let rec = full_access_record("conv-full-access");
    assert_eq!(rec.conversation_id, "conv-full-access");
    assert_eq!(rec.tool, "session");
    assert_eq!(rec.kind, "exec");
    assert_eq!(rec.target, "*");
    assert_eq!(rec.decision, "full-access");
    assert_eq!(rec.scope, "session");

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("permission-log.jsonl");
    rec.append_jsonl(&path).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 1);
    let parsed: newt_core::PermissionRecord = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed, rec);
}

/// `--full-access` / NEWT_FULL_ACCESS=1: `policy_for` builds the session
/// policy from the `full_access` preset (`Caveats::top()`) regardless of
/// the configured preset — and with the override absent, the configured
/// preset rules exactly as before. `top()`'s `exec == Scope::All` is also
/// what empties the #774 floor (`exec_floor_none_only_when_unrestricted_
/// and_no_mode` above), so `--yolo --full-access` covers every command.
#[test]
fn full_access_env_overrides_configured_preset_in_policy_for() {
    use newt_core::caveats::{Caveats as Cav, Scope};
    // Exclusive guard: this test mutates NEWT_FULL_ACCESS, which
    // policy_for reads (alongside the NEWT_*_PATHS grant scans).
    let _g = crate::test_env_guard::env_write_guard();
    let tui = newt_core::TuiConfig::default(); // preset: workspace_dev

    {
        let _off = EnvVar::unset("NEWT_FULL_ACCESS");
        let base = policy_for(Some(tui.clone()), "/ws");
        assert!(
            matches!(base.exec, Scope::Only(_)),
            "override absent ⇒ the configured workspace_dev allowlist rules"
        );
    }

    let _on = EnvVar::set("NEWT_FULL_ACCESS", "1");
    assert_eq!(
        policy_for(Some(tui), "/ws"),
        Cav::top(),
        "override asserted ⇒ the full_access preset, bit-for-bit"
    );
    assert_eq!(
        policy_for(None, "/ws"),
        Cav::top(),
        "the explicit flag overrides even the absent-config read-only default"
    );
}

/// Exec-none caveats, workspace-fenced fs — the shape under which the
/// flag-off confinement tests above pin the fail-closed stub dispatch.
#[cfg(unix)]
fn caveats_no_exec(ws: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::only([ws.to_string_lossy().into_owned()]),
        fs_write: Scope::only([ws.to_string_lossy().into_owned()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    }
}

/// FLAG ON: the command the stub shell fails closed on (see
/// `run_command_out_of_scope_is_denied` above for the flag-off pin) runs
/// on the host shell and returns real output — while a workspace-escape
/// write is STILL denied: yolo is unconfined exec, fenced fs.
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn yolo_runs_exec_unconfined_but_keeps_the_fs_fence() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());

    let out = execute_tool(
        "run_command",
        &serde_json::json!({ "command": "echo yolo-through" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None, // memory_source
        None,
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
    assert_eq!(out, "yolo-through\n");

    let escape = "/definitely-outside-the-fence/escape.txt";
    let out = execute_tool(
        "write_file",
        &serde_json::json!({ "path": escape, "content": "nope" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None, // memory_source
        None,
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
    assert!(
        out.starts_with(&format!(
            "capability denied: fs_write does not permit '{escape}'"
        )),
        "got: {out}"
    );
    // #721: the denial now also carries the model-actionable recovery path.
    assert!(out.contains("request_permissions"), "got: {out}");
    assert!(!std::path::Path::new(escape).exists());
}

/// Precedence (#297): yolo + a #263 gate — exec never prompts (the gate
/// would record an ask; it must stay empty), while an fs denial still
/// prompts exactly as before. `--disable-ocap` >
/// `--prompt-for-permissions` for exec; fs prompting unaffected.
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn yolo_exec_never_prompts_but_fs_prompting_still_works() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_no_exec(ws.path());

    let mut state = PermissionPromptState::default();
    let outside = tempfile::TempDir::new().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "gated contents").unwrap();

    // Every human consult leaves one record in `state.decisions`, so the
    // record count IS the prompt count — zero after the exec call proves
    // the gate was never reached.
    let mut gate = PromptPermissionGate {
        ask_surface: None,
        state: &mut state,
        base: caveats.clone(),
        key_path: None,
        conversation_id: "conv-297".to_string(),
        log_path: None,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: std::time::Duration::from_secs(2),
        cancel: None,
        exit: None,
        ask_human: |_w: &newt_core::tty::PromptWindow,
                    _definition: &newt_interaction::InteractionDefinition| {
            PromptChoice::AllowOnce
        },
    };

    let out = execute_tool(
        "run_command",
        &serde_json::json!({ "command": "echo no-prompt" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
    assert_eq!(out, "no-prompt\n");
    assert!(
        state.decisions.is_empty(),
        "exec under yolo must never reach the gate, got: {:?}",
        state.decisions
    );

    // fs prompting is unaffected: an out-of-fence read consults the gate
    // and the allow-once answer turns the denial into the real contents.
    let mut gate = PromptPermissionGate {
        ask_surface: None,
        state: &mut state,
        base: caveats.clone(),
        key_path: None,
        conversation_id: "conv-297".to_string(),
        log_path: None,
        denials_path: None,
        config_path: None,
        preset_clamp: None,
        danger: danger::DangerTable::builtin(),
        color: false,
        verbose: false,
        authorization_prompts_enabled: true,
        web_decision_timeout: std::time::Duration::from_secs(2),
        cancel: None,
        exit: None,
        ask_human: |_w: &newt_core::tty::PromptWindow,
                    _definition: &newt_interaction::InteractionDefinition| {
            PromptChoice::AllowOnce
        },
    };
    let out = execute_tool(
        "read_file",
        &serde_json::json!({ "path": secret.to_string_lossy() }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut Mcp::empty(),
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
    assert_eq!(out, "gated contents");
    assert_eq!(state.decisions.len(), 1, "the fs denial prompted once");
    assert_eq!(state.decisions[0].kind, "fs_read");
}

/// #307 FLOOR TEST (a) at the TUI seam: with `--disable-ocap` set, a `/posture`
/// readonly preset clamp STOPS the unconfined bypass for a denied exec. The
/// preset's exec floor is threaded as `exec_floor`; `echo` is outside it, so
/// the command does NOT run unconfined — it falls to the confined dispatch
/// (env-seam real shell ⇒ denied). A triage posture is not un-clamped by `--yolo`.
#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn floor_wins_over_disable_ocap_at_the_tui_seam() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _on = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    // #1243 Leg 1: pin safe-subset — this asserts the exec FLOOR wins over
    // --disable-ocap (engine-independent); `echo` is a brush builtin (never
    // spawns → not exec-gated), so the L3-gated default would make this
    // box-dependent.
    let _eng = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let ws = tempfile::TempDir::new().unwrap();
    let base = caveats_no_exec(ws.path());
    // The readonly-triage preset clamp the active posture supplies.
    let clamp = newt_core::NamedPermissionPreset {
        readonly: true,
        ..Default::default()
    }
    .clamp();
    // Effective caveats = base ∩ clamp (already read-only on exec here).
    let effective = base.meet(&clamp);
    let out = execute_tool(
        "run_command",
        &serde_json::json!({ "command": "echo should-not-run" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &effective,
        &mut Mcp::empty(),
        None,
        None,
        None,
        None, // memory_source
        None,
        // The active preset's exec floor — the bypass ceiling.
        Some(&clamp.exec),
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_ne!(out, "should-not-run\n", "the floor must block --yolo");
    assert!(
        out.contains("capability denied"),
        "fell to the confined dispatch and was denied: {out}"
    );
}
