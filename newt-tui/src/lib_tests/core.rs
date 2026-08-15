use super::*;

#[test]
fn openai_surface_probe_skips_plain_http_multiplexers() {
    assert!(!should_probe_openai_surface(
        newt_core::BackendKind::Openai,
        true,
        "model",
        "http://gpu-box:8000",
        Some(newt_core::Serving::Multiplexer),
    ));
    assert!(should_probe_openai_surface(
        newt_core::BackendKind::Openai,
        true,
        "model",
        "http://127.0.0.1:8000",
        Some(newt_core::Serving::Instance),
    ));
    assert!(should_probe_openai_surface(
        newt_core::BackendKind::Openai,
        true,
        "model",
        "https://inference.example.test",
        Some(newt_core::Serving::Multiplexer),
    ));
}

#[test]
fn coauthor_trailer_uses_resolved_identity_email() {
    let id = newt_core::AgentIdentity::default();
    let tr = coauthor_trailer("nemotron-3-nano:30b", &id);
    // Model name credits the work; the email attributes to the resolved
    // harness identity (default: GitHub User https://github.com/newt-agent).
    assert_eq!(
        tr,
        "Co-authored-by: nemotron-3-nano:30b <309460085+newt-agent@users.noreply.github.com>\nModel: nemotron-3-nano:30b"
    );
    assert!(tr.contains(newt_core::DEFAULT_AGENT_EMAIL));
    assert!(
        !tr.contains("noreply@newt-agent.com"),
        "old wrong email is gone"
    );
    assert!(
        !tr.contains("[bot]"),
        "default attribution is the User account, not the App bot"
    );

    let custom = newt_core::AgentIdentity {
        name: "my-agent".into(),
        email: "my-agent@example.com".into(),
        ..newt_core::AgentIdentity::default()
    };
    assert_eq!(
        coauthor_trailer("ornith:35b", &custom),
        "Co-authored-by: ornith:35b <my-agent@example.com>\nModel: ornith:35b"
    );
}

#[test]
fn runtime_context_block_instructs_shell_git_identity() {
    let id = newt_core::AgentIdentity::default();
    let blk = runtime_context_block("m", "http://h", newt_core::BackendKind::Ollama, &id);
    // The shell-git fallback (for a model that bypasses the embedded tool)
    // must carry the resolved User no-reply email.
    assert!(blk.contains("user.email='309460085+newt-agent@users.noreply.github.com'"));
    assert!(blk.contains("user.name='newt-agent'"));
    assert!(blk.contains("git -c user.name="));

    let custom = newt_core::AgentIdentity {
        name: "custom".into(),
        email: "custom@example.com".into(),
        ..newt_core::AgentIdentity::default()
    };
    let custom_blk =
        runtime_context_block("m", "http://h", newt_core::BackendKind::Ollama, &custom);
    assert!(custom_blk.contains("user.email='custom@example.com'"));
    assert!(custom_blk.contains("user.name='custom'"));
}

#[test]
fn workspace_state_block_formats_dirty_snapshot_with_timestamp() {
    let block = format_workspace_state_block(&WorkspaceStateSnapshot {
        timestamp: "2026-07-04T21:23:45-04:00".to_string(),
        branch: Some("feat/help-rollups".to_string()),
        dirty_files: vec![
            "newt-tui/src/help_sections.rs".to_string(),
            "newt-tui/src/lib.rs".to_string(),
        ],
        git_status_available: true,
    });

    assert!(block.starts_with("<workspace_state>\ntimestamp: 2026-07-04T21:23:45-04:00"));
    assert!(block.contains("branch: feat/help-rollups"), "{block}");
    assert!(block.contains("dirty files (2):"), "{block}");
    assert!(
        block.contains("- newt-tui/src/help_sections.rs")
            && block.contains("- newt-tui/src/lib.rs"),
        "{block}"
    );
    assert!(
        block.contains("unlanded local changes exist; do not treat them as upstream-complete work"),
        "{block}"
    );
    assert!(
        block.contains("next completion step: verify, commit, push/open PR, or state blocker"),
        "{block}"
    );
}

#[test]
fn workspace_state_block_formats_clean_snapshot_without_dirty_nudge() {
    let block = format_workspace_state_block(&WorkspaceStateSnapshot {
        timestamp: "2026-07-04T21:23:45-04:00".to_string(),
        branch: Some("main".to_string()),
        dirty_files: Vec::new(),
        git_status_available: true,
    });

    assert!(block.contains("timestamp: 2026-07-04T21:23:45-04:00"));
    assert!(block.contains("branch: main"), "{block}");
    assert!(block.contains("dirty files: none"), "{block}");
    assert!(block.contains("local changes: clean"), "{block}");
    assert!(!block.contains("unlanded local changes exist"), "{block}");
}

#[test]
fn parse_git_porcelain_dirty_files_dedupes_and_tracks_rename_target() {
    let files = parse_git_porcelain_dirty_files(
        " M newt-tui/src/lib.rs\n\
             ?? docs/new file.md\n\
             R  old.rs -> src/new.rs\n\
             M  newt-tui/src/lib.rs\n",
    );

    assert_eq!(
        files,
        vec![
            "newt-tui/src/lib.rs".to_string(),
            "docs/new file.md".to_string(),
            "src/new.rs".to_string(),
        ]
    );
}

#[test]
fn bang_command_strips_and_trims_the_escape() {
    assert_eq!(bang_command("!date"), Some("date"));
    assert_eq!(bang_command("! date"), Some("date"));
    assert_eq!(bang_command("!  pa login  "), Some("pa login"));
    // A pipeline survives intact — it's handed to the shell verbatim.
    assert_eq!(bang_command("! echo hi | wc -c"), Some("echo hi | wc -c"));
}

#[test]
fn bang_command_ignores_non_bang_and_bare_bang() {
    assert_eq!(bang_command("date"), None, "no leading bang");
    assert_eq!(bang_command("/help"), None, "slash is not a bang");
    assert_eq!(bang_command("!"), None, "bare bang has no command");
    assert_eq!(bang_command("!   "), None, "whitespace-only is empty");
    assert_eq!(
        bang_command("the ! is mid-line"),
        None,
        "bang must lead the line"
    );
}

#[test]
#[cfg(not(windows))]
fn bang_shell_is_a_noninteractive_unix_shell() {
    let (shell, flag) = bang_shell();
    // `-c`, NOT `-ic`: an interactive shell's job control suspends newt
    // (SIGTTOU). See bang_shell docs.
    assert_eq!(flag, "-c");
    assert!(!shell.is_empty(), "a shell is always resolved");
}

// A `!`-escape must always RETURN control to newt — the hang bug was
// `.status()` blocking the TUI thread forever on a command that never
// exits. Under the test harness stdin is not a TTY, so `run_bang_escape`
// takes the non-interactive `wait` fallback; these prove that path both
// completes (no hang) and reports exit status without felling the process.
#[test]
#[cfg(not(windows))]
fn run_bang_escape_returns_on_success() {
    // `true` exits 0: the function must return promptly, printing nothing.
    run_bang_escape("true", false, false);
}

#[test]
#[cfg(not(windows))]
fn run_bang_escape_returns_on_nonzero_exit() {
    // `false` exits 1: the function must still return (report + continue),
    // never propagate the failure up as a panic or block the caller.
    run_bang_escape("exit 3", false, false);
}

#[serial_test::serial(real_fs)]
#[test]
#[cfg(windows)]
fn bang_shell_is_a_windows_shell_with_slash_c() {
    let (shell, flag) = bang_shell();
    assert_eq!(flag, "/C");
    assert!(!shell.is_empty(), "a shell is always resolved");
}

fn write_pyo3_binding(root: &std::path::Path, krate: &str, submodule: &str) {
    let dir = root.join(krate).join("src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pyo3_module.rs"),
        format!("#[pyclass(name=\"X\", module=\"newt_agent._newt_agent.{submodule}\")] struct X;"),
    )
    .unwrap();
}

#[serial_test::serial(real_fs)]
#[test]
fn verify_gate_summary_flags_fabrication_and_passes_clean() {
    use newt_core::verify_gate::SurfaceMatch;
    let dir = tempfile::tempdir().unwrap();
    write_pyo3_binding(dir.path(), "newt-core", "core");
    let ws = dir.path().to_str().unwrap();
    std::fs::create_dir_all(dir.path().join("examples")).unwrap();

    // a fabricated import → flagged
    std::fs::write(dir.path().join("examples/bad.py"), "import newt_core\n").unwrap();
    let warn = verify_gate_summary(ws, SurfaceMatch::Exact).expect("a fabrication warning");
    assert!(
        warn.contains("verify_gate") && warn.contains("newt_core"),
        "{warn}"
    );

    // replace with a grounded import → clean (None)
    std::fs::remove_file(dir.path().join("examples/bad.py")).unwrap();
    std::fs::write(
        dir.path().join("examples/ok.py"),
        "from newt_agent._newt_agent.core import X\nimport os\n",
    )
    .unwrap();
    assert!(verify_gate_summary(ws, SurfaceMatch::Exact).is_none());
}

#[serial_test::serial(real_fs)]
#[test]
fn verify_gate_summary_noops_without_pyo3_surface() {
    use newt_core::verify_gate::SurfaceMatch;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("thing.py"), "import newt_core\n").unwrap();
    // no bindings → no authoritative surface → no gating (None)
    assert!(verify_gate_summary(dir.path().to_str().unwrap(), SurfaceMatch::Exact).is_none());
}

#[serial_test::serial(real_fs)]
#[tokio::test]
async fn retry_revert_undoes_only_newts_writes() {
    use newt_core::verify_gate::{SurfaceMatch, WriteLedger};
    let dir = tempfile::tempdir().unwrap();
    write_pyo3_binding(dir.path(), "newt-core", "core");
    let ws = dir.path().to_str().unwrap();
    std::fs::create_dir_all(dir.path().join("examples")).unwrap();

    // a pre-existing grounded file newt will fabricate-edit
    let edited = dir.path().join("examples/edited.py");
    std::fs::write(&edited, "from newt_agent._newt_agent.core import X\n").unwrap();
    // a pre-existing FABRICATING file newt never touches — must be left alone
    let untouched = dir.path().join("examples/untouched.py");
    std::fs::write(&untouched, "import newt_core\n").unwrap();

    // the per-turn ledger records ONLY newt's own writes (the write-tool seam)
    let ledger = std::cell::RefCell::new(WriteLedger::new());
    ledger.borrow_mut().note_before_write(&edited);
    std::fs::write(&edited, "import newt_core\n").unwrap(); // newt fabricate-edits it
    let created = dir.path().join("examples/new.py");
    ledger.borrow_mut().note_before_write(&created);
    std::fs::write(&created, "import newt_coder\n").unwrap(); // newt creates a bad file

    let action = retry_revert(ws, SurfaceMatch::Exact, &ledger)
        .await
        .expect("a revert action");
    assert!(
        action.banner.contains("retry: reverted"),
        "{}",
        action.banner
    );
    // the corrective re-prompt names a fabricated module and the real surface
    assert!(
        action.corrective.contains("newt_core") || action.corrective.contains("newt_coder"),
        "corrective names the bad import: {}",
        action.corrective
    );
    assert!(
        action.corrective.contains("newt_agent._newt_agent.core"),
        "corrective carries the authoritative surface"
    );

    // newt's edit restored to pre-turn bytes; newt's created file deleted
    assert_eq!(
        std::fs::read_to_string(&edited).unwrap(),
        "from newt_agent._newt_agent.core import X\n"
    );
    assert!(!created.exists(), "newt's created fabrication is deleted");
    // the fabricating file newt NEVER wrote is left completely untouched
    assert_eq!(
        std::fs::read_to_string(&untouched).unwrap(),
        "import newt_core\n",
        "a file newt did not write must never be reverted or deleted"
    );

    // re-gate: only `untouched` still fabricates, and newt did not write it,
    // so the revert acts on nothing and reports None.
    assert!(
        retry_revert(ws, SurfaceMatch::Exact, &ledger)
            .await
            .is_none(),
        "nothing newt wrote remains flagged"
    );
}

#[test]
fn retry_step_reprompts_until_the_budget_is_spent() {
    // budget = the re-prompts still allowed this user turn
    assert_eq!(retry_step(2), RetryStep::Reprompt);
    assert_eq!(retry_step(1), RetryStep::Reprompt);
    assert_eq!(retry_step(0), RetryStep::GiveUp);
}

// Regression tests for "the preamble always shows" (splash mode used to
// greet only inside the alternate screen, which vanishes on continue —
// leaving no preamble in scrollback). The header is now rendered by a
// pure function and printed in BOTH modes before chat starts.

#[test]
fn inline_header_color_contains_brand_and_ready_lines() {
    let s = render_inline_header("/w", true);
    assert!(s.contains("Free, friendly, local agentic coder"));
    assert!(s.contains(&format!("v{VERSION}")));
    assert!(s.contains("ready — type a task, /help for commands, /exit to quit"));
    // Text is placed just past the 20-col logo via absolute column moves.
    assert!(s.contains("\x1b[23G"));
}

#[test]
fn inline_header_plain_names_version_and_workspace() {
    let s = render_inline_header("/some/workspace", false);
    assert!(s.contains(&format!("newt v{VERSION}")));
    assert!(s.contains("/some/workspace"));
    // Plain mode must stay safe for dumb terminals and pipes: no ANSI.
    assert!(!s.contains('\x1b'));
}

#[test]
fn inline_header_lists_plugins_when_brand_set() {
    // #507: tests run in PARALLEL and Rust's `set_var`/`remove_var` are not
    // thread-safe — a raw, unguarded mutation here races the `HOME` swap in the
    // cw-400 recovery test (and any other env-touching test), which was the
    // intermittent pre-push-gate flake. Serialize on the shared write guard
    // (the same lock `with_env_vars` and the cw-400 test hold).
    let _g = crate::test_env_guard::env_write_guard();
    unsafe { std::env::set_var("NEWT_BRAND_PLUGINS", "mogul, diagram") };
    let color = render_inline_header("/w", true);
    let plain = render_inline_header("/w", false);
    unsafe { std::env::remove_var("NEWT_BRAND_PLUGINS") };
    assert!(color.contains("plugins:  mogul, diagram"));
    assert!(plain.contains("plugins:  mogul, diagram"));
    // Unset → no plugins line at all.
    assert!(!render_inline_header("/w", false).contains("plugins:"));
}

#[test]
fn inline_header_ends_with_blank_line_before_chat() {
    for color in [true, false] {
        let s = render_inline_header("/w", color);
        assert!(s.ends_with("\n\n"), "header must end with a blank line");
    }
}

#[test]
fn resolve_workspace_falls_back_gracefully() {
    let p = std::path::Path::new("/some/workspace");
    assert_eq!(resolve_workspace(Some(p)), "/some/workspace");
}

#[test]
fn slash_exit_returns_false() {
    for cmd in ["/exit", "/quit"] {
        let result = dispatch_slash(cmd, "/ws", false, false, false).unwrap();
        assert!(!result, "{cmd} should return false (exit)");
    }
}

#[test]
fn config_helpers_read_from_passed_config_not_disk() {
    // Regression (#150): these helpers used to call Config::resolve() — a
    // disk read + TOML parse — on every invocation, several times per turn.
    // They now derive from the &Config threaded in, so run_chat resolves
    // once and reuses it. This test passes a value-bearing Config and proves
    // the returned values come from the argument, not from a config file on
    // disk (which the old, arg-less signatures could not have read).
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            max_tool_rounds: 7,
            workflow_grace_rounds: 4,
            tool_output_lines: 3,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(max_tool_rounds(&cfg), 7);
    assert_eq!(workflow_grace_rounds(&cfg), 4);
    assert_eq!(tool_output_lines(&cfg), 3);
    assert_eq!(resolve_tui(&cfg).map(|t| t.max_tool_rounds), Some(7));
    assert_eq!(resolve_tui(&cfg).map(|t| t.workflow_grace_rounds), Some(4));

    // An empty config yields the documented defaults.
    let empty = newt_core::Config::default();
    assert_eq!(max_tool_rounds(&empty), 40);
    assert_eq!(workflow_grace_rounds(&empty), 5);
    assert_eq!(tool_output_lines(&empty), 20);
    assert_eq!(resolve_tui(&empty), None);
}

#[test]
fn tool_round_limit_commands_parse_expected_forms() {
    assert_eq!(
        parse_tool_round_limit_command("/rounds").unwrap(),
        ToolRoundLimitCommand::Show
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds status").unwrap(),
        ToolRoundLimitCommand::Show
    );
    assert_eq!(
        parse_tool_round_limit_command("/tool-rounds 50").unwrap(),
        ToolRoundLimitCommand::Set(50)
    );
    assert_eq!(
        parse_tool_round_limit_command("/max-rounds double").unwrap(),
        ToolRoundLimitCommand::Double
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds x2").unwrap(),
        ToolRoundLimitCommand::Double
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds reset").unwrap(),
        ToolRoundLimitCommand::Reset
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds default").unwrap(),
        ToolRoundLimitCommand::Reset
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds auto").unwrap(),
        ToolRoundLimitCommand::Reset
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds config").unwrap(),
        ToolRoundLimitCommand::Configured
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds unlimited").unwrap(),
        ToolRoundLimitCommand::Unlimited
    );
    assert_eq!(
        parse_tool_round_limit_command("/rounds run until finished").unwrap(),
        ToolRoundLimitCommand::Unlimited
    );

    assert!(parse_tool_round_limit_command("/roundsx 10").is_err());
    assert!(parse_tool_round_limit_command("/rounds 0").is_err());
    assert!(parse_tool_round_limit_command("/rounds 10001").is_err());
    assert!(parse_tool_round_limit_command("/rounds many").is_err());
}

#[test]
fn tool_round_limit_override_resolves_and_reports() {
    use newt_core::Tenacity;

    assert_eq!(effective_tool_round_limit(25, None, None), 25);
    assert_eq!(
        effective_tool_round_limit(25, Some(Tenacity::Standard), None),
        25
    );
    assert_eq!(
        effective_tool_round_limit(25, Some(Tenacity::Relentless), None),
        EFFECTIVELY_UNLIMITED_TOOL_ROUNDS,
        "an explicitly selected relentless posture should not hit the ordinary safety cap"
    );
    assert_eq!(
        effective_tool_round_limit(25, Some(Tenacity::Relentless), Some(7)),
        7,
        "a deliberate /rounds override remains the outermost limit"
    );
    assert_eq!(effective_tool_round_limit(25, None, Some(50)), 50);
    assert_eq!(double_tool_round_limit(25), 50);
    assert_eq!(
        double_tool_round_limit(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS),
        EFFECTIVELY_UNLIMITED_TOOL_ROUNDS
    );
    assert_eq!(
        double_tool_round_limit(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS * 2),
        EFFECTIVELY_UNLIMITED_TOOL_ROUNDS * 2,
        "doubling must never lower a larger configured/session value"
    );
    assert_eq!(
        tool_round_limit_status(25, None, None),
        "tool-call round limit: 25 (config/model default)"
    );
    assert_eq!(
        tool_round_limit_status(25, None, Some(50)),
        "tool-call round limit: 50 this session (config/model default 25)"
    );
    assert_eq!(
        tool_round_limit_status(25, Some(Tenacity::Relentless), None),
        "tool-call round limit: 10000 (effectively unlimited; explicit relentless tenacity; config/model default 25)"
    );
    assert_eq!(
        tool_round_limit_status(25, Some(Tenacity::Relentless), Some(7)),
        "tool-call round limit: 7 this session (explicit relentless tenacity default 10000 (effectively unlimited); config/model default 25)"
    );
    assert!(
        tool_round_limit_status(25, None, Some(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS))
            .contains("effectively unlimited")
    );

    let relentless = Some(Tenacity::Relentless);
    assert_eq!(
        apply_tool_round_limit_command(25, relentless, None, ToolRoundLimitCommand::Configured,),
        Some(25),
        "config explicitly bypasses the tenacity-derived default"
    );
    assert_eq!(
        apply_tool_round_limit_command(25, relentless, Some(25), ToolRoundLimitCommand::Reset,),
        None,
        "reset returns to automatic derivation"
    );
    assert_eq!(
        effective_tool_round_limit(
            25,
            relentless,
            apply_tool_round_limit_command(25, relentless, Some(25), ToolRoundLimitCommand::Reset,),
        ),
        EFFECTIVELY_UNLIMITED_TOOL_ROUNDS
    );
    assert_eq!(
        apply_tool_round_limit_command(20_000, relentless, None, ToolRoundLimitCommand::Unlimited,),
        Some(20_000),
        "unlimited must not lower an already larger effective limit"
    );
    assert!(
        tool_round_limit_status(20_000, relentless, None).contains("explicit relentless tenacity"),
        "status keeps operator provenance even when config already exceeds the sentinel"
    );
}

#[test]
fn psyche_apply_summary_includes_the_effective_round_source() {
    use newt_core::Tenacity;

    let summary = psyche_apply_summary(
        "psyche: contemplating + relentless",
        40,
        Some(Tenacity::Relentless),
        Some(7),
    );
    assert!(
        summary.starts_with("psyche: contemplating + relentless"),
        "{summary}"
    );
    assert!(
        summary.contains("tool-call round limit: 7 this session"),
        "{summary}"
    );
    assert!(
        summary.contains("relentless tenacity default 10000"),
        "{summary}"
    );
}

#[test]
fn spill_commands_parse_expected_forms() {
    assert_eq!(parse_spill_command("/spill").unwrap(), SpillCommand::Status);
    assert_eq!(
        parse_spill_command("/spill status").unwrap(),
        SpillCommand::Status
    );
    assert_eq!(
        parse_spill_command("/spill 7").unwrap(),
        SpillCommand::Set(7)
    );
    assert_eq!(
        parse_spill_command("/spill 0").unwrap(),
        SpillCommand::Set(0)
    );
    assert_eq!(
        parse_spill_command("/spill reset").unwrap(),
        SpillCommand::Reset
    );
    // #1640 Layer 1: the mode switches.
    assert_eq!(
        parse_spill_command("/spill summary").unwrap(),
        SpillCommand::Summary
    );
    assert_eq!(
        parse_spill_command("/spill excerpt").unwrap(),
        SpillCommand::Excerpt
    );
    assert!(parse_spill_command("/spillage 7").is_err());
    assert!(parse_spill_command("/spill many").is_err());
}

/// #1640 Layer 1: the summary default follows the SURFACE — rich collapses
/// (its /spill vocabulary can recover detail), lean never does (it shows full
/// output) — and the session override wins over both.
#[test]
fn spill_summary_defaults_to_the_surface_and_override_wins() {
    assert!(effective_spill_summary(true, None), "rich collapses");
    assert!(
        !effective_spill_summary(false, None),
        "lean never collapses"
    );
    assert!(
        !effective_spill_summary(true, Some(false)),
        "/spill excerpt"
    );
    assert!(effective_spill_summary(false, Some(true)), "/spill summary");

    // `/spill status` names the mode — and its escape hatch — when it is on,
    // and stays byte-identical to the historical string when it is off.
    let on = spill_status(3, None, true, true, SpillEligibility::Available);
    assert!(
        on.contains("results collapse to a summary line (/spill excerpt restores rows)"),
        "{on}"
    );
    let off = spill_status(3, None, false, true, SpillEligibility::Available);
    assert!(!off.contains("summary line"), "{off}");
}

#[test]
fn spill_status_never_claims_a_collapse_that_cannot_engage() {
    // #1663 review F3/F9: the mode clause is guarded like the renderer is.
    // (i) lean: committed rendering is forced full, so even summary=true
    // (a /spill summary override) must not claim collapse — and the status
    // says plainly that committed results print in full there.
    let lean = spill_status(3, None, true, false, SpillEligibility::Available);
    assert!(!lean.contains("summary line"), "{lean}");
    assert!(
        lean.contains("committed results always print in full"),
        "{lean}"
    );
    // (ii) rich after /spill 0: unbounded view never collapses.
    let unbounded = spill_status(3, Some(0), true, true, SpillEligibility::Available);
    assert!(!unbounded.contains("summary line"), "{unbounded}");
    // Rich with rows > 0 keeps the honest claim (guard must not over-suppress).
    let rich = spill_status(3, None, true, true, SpillEligibility::Available);
    assert!(rich.contains("summary line"), "{rich}");
    assert!(!rich.contains("print in full"), "{rich}");
}

#[test]
fn spill_reset_clears_both_session_knobs() {
    // #1663 review F12: Reset returns BOTH knobs to surface defaults; the
    // other forms each touch only the knob they own.
    let mut lines = Some(7usize);
    let mut summary = Some(true);
    apply_spill_command(SpillCommand::Reset, &mut lines, &mut summary);
    assert_eq!((lines, summary), (None, None), "Reset clears BOTH");

    apply_spill_command(SpillCommand::Set(5), &mut lines, &mut summary);
    assert_eq!(
        (lines, summary),
        (Some(5), None),
        "Set leaves the mode alone"
    );

    apply_spill_command(SpillCommand::Summary, &mut lines, &mut summary);
    assert_eq!((lines, summary), (Some(5), Some(true)), "Summary sets mode");

    apply_spill_command(SpillCommand::Excerpt, &mut lines, &mut summary);
    assert_eq!((lines, summary), (Some(5), Some(false)));

    apply_spill_command(SpillCommand::Status, &mut lines, &mut summary);
    assert_eq!(
        (lines, summary),
        (Some(5), Some(false)),
        "Status is read-only"
    );
}

#[test]
fn search_commands_parse_expected_forms() {
    assert_eq!(
        parse_search_command("/search").unwrap(),
        SearchCommand::Help
    );
    assert_eq!(
        parse_search_command("/search help").unwrap(),
        SearchCommand::Help
    );
    assert_eq!(
        parse_search_command("/search auth handshake").unwrap(),
        SearchCommand::Query("auth handshake".into())
    );
    assert_eq!(
        parse_search_command("/search preview").unwrap(),
        SearchCommand::Preview(1)
    );
    assert_eq!(
        parse_search_command("/search preview 3").unwrap(),
        SearchCommand::Preview(3)
    );
    assert_eq!(
        parse_search_command("/search model").unwrap(),
        SearchCommand::Model
    );
    assert_eq!(
        parse_search_command("/search rejects").unwrap(),
        SearchCommand::Rejects
    );
    assert_eq!(
        parse_search_command("/search pin 2").unwrap(),
        SearchCommand::Pin(2)
    );
    assert_eq!(
        parse_search_command("/search exclude 1").unwrap(),
        SearchCommand::Exclude(1)
    );
    assert_eq!(
        parse_search_command("/search status").unwrap(),
        SearchCommand::Status
    );
    assert_eq!(
        parse_search_command("/search clear").unwrap(),
        SearchCommand::Clear
    );
    assert!(parse_search_command("/searching foo").is_err());
    assert!(parse_search_command("/search pin").is_err());
}

/// #1434: `--trace` seeds the detail knob, so there is ONE source of truth and
/// no launch-vs-runtime phase.
///
/// This is the bug pi shipped, written down so newt cannot ship it.
/// `modes/interactive/interactive-mode.ts` computes
/// `options.verbose || toolOutputExpanded` at one call site, and nothing ever
/// initializes `toolOutputExpanded` from `options.verbose`. So after
/// `pi --verbose` the state is "expanded" for display purposes but `false` in
/// the variable, and **the first toggle press expands again instead of
/// collapsing** — the control moves the wrong way on its first use, which is
/// the worst possible first impression for a toggle.
///
/// Seeding removes the second variable, so the phase cannot exist.
#[test]
fn trace_seeds_the_detail_knob_so_the_first_toggle_collapses() {
    const CONFIGURED: usize = 3;

    // Without --trace the knob simply follows config.
    assert_eq!(initial_spill_override(false), None);
    assert_eq!(
        effective_spill_lines(CONFIGURED, initial_spill_override(false)),
        CONFIGURED
    );

    // With --trace the session STARTS unbounded …
    let seeded = initial_spill_override(true);
    assert_eq!(seeded, Some(0), "--trace means show me everything");
    assert_eq!(effective_spill_lines(CONFIGURED, seeded), 0);

    // … and the first toggle press must COLLAPSE, not expand again.
    let after_first_press = toggle_spill_detail(seeded, CONFIGURED);
    assert_eq!(
        effective_spill_lines(CONFIGURED, after_first_press),
        CONFIGURED,
        "the first toggle after --trace went the wrong way — this is pi's phase \
         bug, and seeding exists precisely to make it unrepresentable"
    );

    // And it flips back.
    let after_second_press = toggle_spill_detail(after_first_press, CONFIGURED);
    assert_eq!(effective_spill_lines(CONFIGURED, after_second_press), 0);
}

/// #1434: the toggle is defined over the same `Option<usize>` `/spill` mutates,
/// so a `/spill N` mid-session and a toggle cannot disagree.
#[test]
fn detail_toggle_shares_state_with_the_spill_command() {
    // Operator sets an explicit height, then toggles.
    let explicit = Some(7);
    let expanded = toggle_spill_detail(explicit, 3);
    assert_eq!(effective_spill_lines(3, expanded), 0, "toggle expands");

    // Toggling back returns to the CONFIGURED height, not the explicit 7 —
    // the toggle is a two-state control, and `/spill 7` is how you get 7 back.
    let collapsed = toggle_spill_detail(expanded, 3);
    assert_eq!(effective_spill_lines(3, collapsed), 3);

    // A configured height of 0 must not make the toggle a no-op that reads as
    // a broken control.
    let from_zero_config = toggle_spill_detail(Some(0), 0);
    assert!(effective_spill_lines(0, from_zero_config) > 0);
}

#[test]
fn spill_override_resolves_and_reports_live_capability() {
    assert_eq!(effective_spill_lines(3, None), 3);
    assert_eq!(effective_spill_lines(3, Some(7)), 7);
    assert_eq!(effective_spill_lines(3, Some(0)), 0);
    assert_eq!(
        spill_status(3, None, false, true, SpillEligibility::Available),
        "spill rows: 3 (config default; live interaction available)"
    );
    // #1412: the unavailable arm now NAMES the refusing gate instead of saying
    // only "unavailable" — that silence is why a stale install was reported as
    // a vanished feature.
    assert_eq!(
        spill_status(3, Some(7), false, true, SpillEligibility::StdoutNotTty),
        "spill rows: 7 this session (config default 3; live interaction unavailable: \
         stdout is not a terminal (piped or redirected))"
    );
    assert_eq!(
        spill_status(3, Some(0), false, true, SpillEligibility::Available),
        "spill rows: unbounded this session (config default 3; live viewport disabled: \
         spill_lines is 0 (/spill <n> raises it))"
    );
}

/// #1640: the committed tool-output excerpt is collapsed ONLY on the rich
/// surface — the surface that can recover the hidden lines through an
/// interactive viewport. The lean surface (the plain scroller) has no scroll
/// region, so its excerpt is the whole record and must be shown in full.
#[test]
fn committed_excerpt_is_full_on_lean_and_collapsed_on_rich() {
    const CONFIGURED: usize = 3;

    // Rich keeps the configured height (a viewport backs the truncation)…
    assert_eq!(committed_spill_lines(true, CONFIGURED), CONFIGURED);
    // …and honours an explicit /spill 0 (unbounded) unchanged.
    assert_eq!(committed_spill_lines(true, 0), 0);

    // Lean is ALWAYS unbounded regardless of the configured height — there is
    // no scrollable region on the plain scroller (#1640), so a "▲ N more lines
    // above · /spill N" excerpt would hide output the surface cannot reveal.
    // 0 is `spill_view_lines`' existing "print every line" contract (proven in
    // newt-core's own display tests), so lean reuses that path unchanged.
    assert_eq!(committed_spill_lines(false, CONFIGURED), 0);
    assert_eq!(committed_spill_lines(false, 42), 0);
    assert_eq!(committed_spill_lines(false, 0), 0);
}

/// #1412: every refusal reason reaches the operator, and zero rows is reported
/// as a config choice rather than a broken terminal.
#[test]
fn spill_status_names_the_gate_that_refused() {
    for (eligibility, needle) in [
        (
            SpillEligibility::UnsupportedPlatform,
            "unsupported platform",
        ),
        (SpillEligibility::FeatureDisabled, "`live-spill` feature"),
        (SpillEligibility::StdinNotTty, "stdin is not a terminal"),
        (SpillEligibility::StdoutNotTty, "stdout is not a terminal"),
        (SpillEligibility::TermDumb, "TERM=dumb"),
    ] {
        let status = spill_status(3, None, false, true, eligibility);
        assert!(
            status.contains(needle),
            "{eligibility:?} must surface {needle:?} to the operator, got: {status}"
        );
    }

    // Rows are a separate axis from capability: a capable terminal with zero
    // rows must not be reported as an incapable terminal.
    let zero = spill_status(0, None, false, true, SpillEligibility::Available);
    assert!(zero.contains("spill_lines is 0"), "{zero}");
    assert!(
        !zero.contains("unavailable"),
        "zero rows is a config choice, not a broken terminal: {zero}"
    );
}

/// #1412: precedence is part of the contract. When several gates refuse at
/// once, the one the operator cannot change without a different binary must
/// win — otherwise `/spill` sends someone to fix `TERM` on a build that never
/// compiled the feature in.
#[test]
fn spill_eligibility_reports_the_most_fundamental_refusal_first() {
    // Every gate failing at once: platform outranks all.
    assert_eq!(
        spill_eligibility_for(false, false, false, false, Some("dumb")),
        SpillEligibility::UnsupportedPlatform
    );
    // Supported platform, everything else failing: the cargo feature is next.
    assert_eq!(
        spill_eligibility_for(true, false, false, false, Some("dumb")),
        SpillEligibility::FeatureDisabled
    );
    // Then the invocation-shaped gates, stdin before stdout.
    assert_eq!(
        spill_eligibility_for(true, true, false, false, Some("dumb")),
        SpillEligibility::StdinNotTty
    );
    assert_eq!(
        spill_eligibility_for(true, true, true, false, Some("dumb")),
        SpillEligibility::StdoutNotTty
    );
    // Only the terminal's own declaration remains.
    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("dumb")),
        SpillEligibility::TermDumb
    );
    assert_eq!(
        spill_eligibility_for(true, true, true, true, Some("xterm-256color")),
        SpillEligibility::Available
    );
    // A terminal that declares nothing is not "dumb".
    assert_eq!(
        spill_eligibility_for(true, true, true, true, None),
        SpillEligibility::Available
    );
}

/// #1412: the boolean façade must agree with the typed answer at every input,
/// so the mouse tier cannot drift from `/spill`'s account of the same gates.
#[test]
fn capable_bool_agrees_with_typed_eligibility() {
    for platform in [true, false] {
        for feature in [true, false] {
            for stdin in [true, false] {
                for stdout in [true, false] {
                    for term in [Some("xterm"), Some("dumb"), None] {
                        assert_eq!(
                            live_spill_capable_for(platform, feature, stdin, stdout, term),
                            spill_eligibility_for(platform, feature, stdin, stdout, term)
                                == SpillEligibility::Available,
                            "disagreement at platform={platform} feature={feature} \
                             stdin={stdin} stdout={stdout} term={term:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn live_spill_requires_a_supported_interactive_terminal() {
    assert!(live_spill_capable_for(
        true,
        true,
        true,
        true,
        Some("xterm-256color")
    ));
    assert!(!live_spill_capable_for(
        false,
        true,
        true,
        true,
        Some("xterm")
    ));
    assert!(!live_spill_capable_for(
        true,
        false,
        true,
        true,
        Some("xterm")
    ));
    assert!(!live_spill_capable_for(
        true,
        true,
        false,
        true,
        Some("xterm")
    ));
    assert!(!live_spill_capable_for(
        true,
        true,
        true,
        false,
        Some("xterm")
    ));
    assert!(!live_spill_capable_for(
        true,
        true,
        true,
        true,
        Some("dumb")
    ));
}

// #1303 acceptance 1a: the mouse tier is `live_spill_capable()` AND an explicit
// opt-in — a strict superset gate. Mouse events arrive on stdin, so a
// stdin-piped session (stdin_terminal=false) must never enable capture even
// when stdout is a TTY.
#[cfg(feature = "live-spill")]
#[test]
fn mouse_tier_requires_optin_and_a_supported_interactive_terminal() {
    // Every predicate satisfied — INCLUDING the explicit opt-in — is the only
    // way the mouse tier turns on.
    assert!(mouse_capable_for(
        true,
        true,
        true,
        true,
        Some("xterm-256color"),
        true
    ));
    // Opt-in off ⇒ never capable, even on a flawless interactive TTY.
    assert!(!mouse_capable_for(
        true,
        true,
        true,
        true,
        Some("xterm"),
        false
    ));
    // Each base predicate false-flips to incapable (opt-in held on).
    assert!(!mouse_capable_for(
        false,
        true,
        true,
        true,
        Some("xterm"),
        true
    )); // platform
    assert!(!mouse_capable_for(
        true,
        false,
        true,
        true,
        Some("xterm"),
        true
    )); // feature
    assert!(!mouse_capable_for(
        true,
        true,
        false,
        true,
        Some("xterm"),
        true
    )); // stdin TTY
    assert!(!mouse_capable_for(
        true,
        true,
        true,
        false,
        Some("xterm"),
        true
    )); // stdout TTY
    assert!(!mouse_capable_for(
        true,
        true,
        true,
        true,
        Some("dumb"),
        true
    )); // TERM=dumb
}

// #1303: the opt-in reads from `[tui] mouse_viewport`, default off.
#[cfg(feature = "live-spill")]
#[test]
fn mouse_viewport_optin_reads_from_config_default_off() {
    let empty = newt_core::Config::default();
    assert!(!mouse_viewport(&empty), "default is off");
    let opted_in = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            mouse_viewport: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(mouse_viewport(&opted_in));
}

#[test]
fn slash_help_returns_true() {
    assert!(dispatch_slash("/help", "/ws", false, false, false).unwrap());
}

#[test]
fn help_request_recognizes_every_form() {
    assert_eq!(help_request("/models --help").as_deref(), Some("models"));
    assert_eq!(help_request("/models -h").as_deref(), Some("models"));
    assert_eq!(help_request("/probe help").as_deref(), Some("probe"));
    assert_eq!(help_request("/help models").as_deref(), Some("models"));
    // Bare /help is the full list, not a per-command page.
    assert_eq!(help_request("/help"), None);
    // Ordinary commands (and their args) are not help requests.
    assert_eq!(help_request("/models capabilities"), None);
    assert_eq!(help_request("/model qwen3:30b"), None);
}

#[test]
fn help_request_treats_start_and_rename_titles_as_titles_not_help() {
    // #1030: /start and /rename take a free-text title, so a help token INSIDE
    // the title must not be swallowed by the help interceptor.
    assert_eq!(help_request("/start help me debug"), None);
    assert_eq!(help_request("/rename fix the -h flag"), None);
    assert_eq!(help_request("/start My Title"), None);
    // A SOLE help token still asks for help, folded to the documenting page.
    assert_eq!(help_request("/start --help").as_deref(), Some("new"));
    assert_eq!(help_request("/rename -h").as_deref(), Some("conversation"));
}

#[test]
fn command_help_covers_every_listed_command_and_folds_aliases() {
    for cmd in [
        "models",
        "model",
        "backend",
        "backends",
        "thinking",
        "probe",
        "memory",
        "compress",
        "summarizer",
        "rounds",
        "tool-rounds",
        "max-rounds",
        "remember",
        "new",
        "end",
        "restart",
        "conversation",
        "recall",
        "persona",
        "crew",
        "dgx",
        "permissions",
        "mode",
        "posture",
        "loadout",
        "plan",
        "status",
        "info",
        "docs",
        "workspace",
        "spill",
        "config",
        "prompt",
        "allow",
        "vi",
        "emacs",
        "nano",
        "version",
        "exit",
        "quit",
        "help",
    ] {
        assert!(command_help_page(cmd).is_some(), "no help page for /{cmd}");
    }
    assert!(command_help_page("bogus").is_none());
    // Aliases share one page.
    assert_eq!(command_help_page("restart"), command_help_page("new"));
    assert_eq!(command_help_page("emacs"), command_help_page("vi"));
    assert_eq!(command_help_page("quit"), command_help_page("exit"));
    assert_eq!(command_help_page("plan"), command_help_page("roadmap"));
    assert_eq!(command_help_page("allow"), command_help_page("permissions"));
    assert!(help_lines().iter().any(|l| l.contains("/loadout")));
    assert!(help_lines().iter().any(|l| l.contains("/status")));
    assert!(help_lines().iter().any(|l| l.contains("/info")));
    assert!(help_lines().iter().any(|l| l.contains("/docs")));
    assert!(help_lines().iter().any(|l| l.contains("/allow")));
    assert!(help_lines().iter().any(|l| l.contains("/plan")));
    // #1665: /psyche is the one advertised dial surface; the retired top-level
    // /tenacity + /cognition lines are gone from the corpus (their help PAGES
    // survive as redirects for /help tenacity muscle memory).
    assert!(help_lines().iter().any(|l| l.contains("/psyche")));
    assert!(!help_lines()
        .iter()
        .any(|l| l.contains("/tenacity") || l.contains("/cognition")));
    let tenacity_page = command_help_page("tenacity").expect("tenacity redirect page");
    assert!(
        tenacity_page.contains("/psyche tenacity"),
        "{tenacity_page}"
    );
    let cognition_page = command_help_page("cognition").expect("cognition redirect page");
    assert!(
        cognition_page.contains("/psyche cognition"),
        "{cognition_page}"
    );
    let spill_help = command_help_page("spill").expect("spill help page");
    assert!(spill_help.contains("Space or Enter"));
    assert!(spill_help.contains("⧉"));
    assert!(spill_help.contains("▣"));
    // The unknown-topic miss is reported (returns false).
    assert!(!print_command_help("bogus", false, false, false));
}

/// ANTI-DRIFT: the startup-free `render_help` path (behind `newt help`) and the
/// interactive TUI's Markdown-off fallback must stay pinned to the
/// single-source-of-truth corpora (`help_lines` / `command_help_page`):
/// `print_newt` == `println!(newt_line(...))` (a `newt_line` + `\n`), then one
/// corpus line per `\n`-terminated row.
#[test]
fn render_help_is_byte_identical_to_the_plain_help_corpus() {
    for &(color, verbose) in &[(false, false), (true, false), (false, true), (true, true)] {
        // Bare `/help` — the full command list.
        let mut expected = newt_line("Available commands:", color, verbose);
        expected.push('\n');
        for line in help_lines() {
            expected.push_str(line);
            expected.push('\n');
        }
        assert_eq!(
            render_help(None, color, verbose),
            expected,
            "render_help(None) drifted from the help_lines corpus (color={color}, verbose={verbose})"
        );

        // Per-command detail pages: a representative command, an alias that
        // folds to a shared page, and the #548 `/dgx` target.
        for cmd in ["dgx", "help", "exit", "quit", "emacs"] {
            let page = command_help_page(cmd).expect("help page exists");
            let mut want = newt_line(
                &format!("/{} help", canonical_help_topic(cmd)),
                color,
                verbose,
            );
            want.push('\n');
            for line in page.lines() {
                want.push_str(line);
                want.push('\n');
            }
            assert_eq!(
                render_help(Some(cmd), color, verbose),
                want,
                "render_help(Some({cmd:?})) drifted from command_help_page"
            );
        }

        // Unknown topic renders the one-line miss (never empty, never a panic).
        let mut miss = newt_line(
            "no help for '/bogus' — /help lists every command",
            color,
            verbose,
        );
        miss.push('\n');
        assert_eq!(render_help(Some("bogus"), color, verbose), miss);
    }
}

/// RichTUI help is a Markdown document, governed by the same `[tui].markdown`
/// policy as assistant output. The plain/startup-free corpus remains available
/// when rendering is disabled.
#[test]
fn rich_help_uses_the_default_markdown_policy_and_honors_off() {
    let default_cfg = newt_core::Config::default();
    let markdown = markdown_enabled(&default_cfg, true, None);
    assert!(markdown, "RichTUI defaults to rendered Markdown");

    let rendered = render_help_for_tui(None, true, false, markdown, 100);
    assert!(
        rendered.starts_with("▸  \x1b[1m\x1b[38;2;220;60;20mAvailable"),
        "the narrator prefix must stay outside the Markdown parser: {rendered:?}"
    );
    assert!(!rendered.contains("## Available commands"));
    assert!(rendered.contains("commands"));
    assert!(rendered.contains("• "));
    assert!(rendered.contains("/models"));

    let cfg_off = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            markdown: newt_core::MarkdownMode::Off,
            ..Default::default()
        }),
        ..Default::default()
    };
    let markdown = markdown_enabled(&cfg_off, true, None);
    assert!(!markdown);
    assert_eq!(
        render_help_for_tui(None, true, false, markdown, 100),
        render_help(None, true, false)
    );
}

#[test]
fn slash_version_returns_true() {
    assert!(dispatch_slash("/version", "/ws", false, false, false).unwrap());
}

#[test]
fn slash_workspace_returns_true() {
    assert!(dispatch_slash("/workspace", "/ws", false, false, false).unwrap());
}

#[test]
fn permission_audit_lines_lists_newest_entries_and_ignores_bad_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("permission-log.jsonl");
    std::fs::write(
        &path,
        "\
{\"ts_claim\":\"t0\",\"conversation_id\":\"c\",\"tool\":\"run_command\",\"kind\":\"exec\",\"target\":\"/bin/echo\",\"decision\":\"allow\",\"scope\":\"session\"}\n\
this is not json\n\
{\"ts_claim\":\"t1\",\"conversation_id\":\"c\",\"tool\":\"run_command\",\"kind\":\"net\",\"target\":\"https://example.com\",\"decision\":\"deny\",\"scope\":\"once\"}\n\
",
    )
    .unwrap();

    let lines = permission_audit_lines(&path, 5);
    assert_eq!(
        lines.first(),
        Some(&"permission audit: 2 of 2 (newest first)".to_string())
    );
    assert!(lines[1].contains("deny"));
    assert!(lines[1].contains("once"));
    assert!(lines[1].contains("net"));
    assert!(lines[1].contains("https://example.com"));
    assert!(lines[2].contains("allow"));

    let limited = permission_audit_lines(&path, 1);
    assert_eq!(
        limited,
        vec![
            "permission audit: 1 of 2 (newest first)".to_string(),
            "  deny    once      net      https://example.com via run_command".to_string()
        ]
    );
}

#[test]
fn help_lists_loadout_command() {
    assert!(help_lines().iter().any(|l| l.contains("/loadout")));
}

#[test]
fn loadout_view_renders_declared_and_resolved() {
    let l = newt_core::Loadout {
        provider: Some("dgx".into()),
        model: Some("nemotron@deep".into()),
        kit: Some("nemotron".into()),
        profile: Some("nemotron".into()),
        role: Some("python-developer".into()),
        settings: Some(newt_core::LoadoutSettings {
            num_ctx: Some(24576),
            framing: Some("Ship small.".into()),
        }),
    };
    let pick = newt_core::config::ProfilePick {
        name: "nemotron".into(),
        via: newt_core::config::PickVia::Bundle("nemotron".into()),
    };
    let view = LoadoutView {
        name: Some("dev"),
        loadout: Some(&l),
        inf_url: "http://dgx:11434",
        inf_model: "nemotron-3:33b",
        profile_pick: Some(&pick),
        persona: Some("python-developer"),
    };
    let out = view.render();
    assert!(out.contains("Active loadout: dev"), "{out}");
    // declared axes
    assert!(
        out.contains("declared:") && out.contains("nemotron@deep"),
        "{out}"
    );
    assert!(
        out.contains("24576") && out.contains("Ship small."),
        "{out}"
    );
    // resolved effect + profile provenance
    assert!(out.contains("nemotron-3:33b @ http://dgx:11434"), "{out}");
    assert!(out.contains("via bundle 'nemotron'"), "{out}");
    assert!(out.contains("python-developer"), "{out}");
}

#[test]
fn loadout_view_renders_when_none_active() {
    let view = LoadoutView {
        name: None,
        loadout: None,
        inf_url: "http://localhost:11434",
        inf_model: "llama3.1:8b",
        profile_pick: None,
        persona: None,
    };
    let out = view.render();
    assert!(out.contains("No loadout active."), "{out}");
    assert!(
        out.contains("llama3.1:8b @ http://localhost:11434"),
        "{out}"
    );
    // unset axes are explicit, not blank
    assert!(out.contains("profile") && out.contains("(none)"), "{out}");
}

#[test]
fn slash_config_returns_true() {
    // Dumps the resolved config (secrets redacted) and keeps the session alive.
    assert!(dispatch_slash("/config", "/ws", false, false, false).unwrap());
}

#[test]
fn help_lists_config_command() {
    assert!(help_lines().iter().any(|l| l.contains("/config")));
}

#[test]
fn slash_unknown_returns_true() {
    assert!(dispatch_slash("/notacommand", "/ws", false, false, false).unwrap());
}

#[test]
fn retired_dials_through_the_real_router_mutate_nothing() {
    // #1665 review: the earlier no-mutation test called settings::dispatch
    // directly, bypassing lib.rs routing. Drive the FULL router. Residual gap,
    // stated honestly: with no stdout capture in this suite, deleting the
    // "tenacity"/"cognition" tokens from dispatch_slash degrades them to the
    // unknown-command print — observably identical here. What this DOES pin:
    // the router path never half-applies a retired dial, whatever it prints.
    use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
    use newt_core::tenacity::{cli_tenacity, set_cli_tenacity, Tenacity};
    let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
    set_cli_cognition(CognitionOverride::Unset);
    set_cli_tenacity(Tenacity::Standard);
    assert!(dispatch_slash("/tenacity relentless", "/ws", false, false, false).unwrap());
    assert!(dispatch_slash("/cognition contemplating", "/ws", false, false, false).unwrap());
    assert_eq!(cli_tenacity(), Some(Tenacity::Standard));
    assert_eq!(cli_cognition(), CognitionOverride::Unset);
    // And the surviving /psyche text setters DO mutate through the router —
    // proving the routing distinction, not just shared inertness.
    assert!(dispatch_slash("/psyche tenacity relentless", "/ws", false, false, false).unwrap());
    assert_eq!(cli_tenacity(), Some(Tenacity::Relentless));
    set_cli_tenacity(Tenacity::Standard);
}

#[test]
fn slash_dgx_no_subcmd_returns_true() {
    assert!(dispatch_slash("/dgx", "/ws", false, false, false).unwrap());
}

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12
