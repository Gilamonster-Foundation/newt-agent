use super::*;

#[test]
fn coauthor_trailer_uses_the_bot_github_email() {
    let tr = coauthor_trailer("nemotron-3-nano:30b");
    // Model name credits the work; the email attributes to the newt-agent[bot]
    // GitHub App (the fix — the old noreply@newt-agent.com attributed nowhere).
    assert_eq!(
        tr,
        "Co-authored-by: nemotron-3-nano:30b <293447090+newt-agent[bot]@users.noreply.github.com>"
    );
    assert!(tr.contains(newt_core::DEFAULT_AGENT_EMAIL));
    assert!(
        !tr.contains("noreply@newt-agent.com"),
        "old wrong email is gone"
    );
}

#[test]
fn runtime_context_block_instructs_shell_git_identity() {
    let blk = runtime_context_block("m", "http://h", newt_core::BackendKind::Ollama);
    // The shell-git fallback (for a model that bypasses the embedded tool)
    // must carry the canonical bot email.
    assert!(blk.contains("user.email='293447090+newt-agent[bot]@users.noreply.github.com'"));
    assert!(blk.contains("git -c user.name="));
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
    assert!(s.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
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
        let result = dispatch_slash(cmd, "/ws", false, false).unwrap();
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
    assert_eq!(max_tool_rounds(&empty), 25);
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
    assert_eq!(effective_tool_round_limit(25, None), 25);
    assert_eq!(effective_tool_round_limit(25, Some(50)), 50);
    assert_eq!(double_tool_round_limit(25), 50);
    assert_eq!(
        double_tool_round_limit(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS),
        EFFECTIVELY_UNLIMITED_TOOL_ROUNDS
    );
    assert_eq!(
        tool_round_limit_status(25, None),
        "tool-call round limit: 25 (config/model default)"
    );
    assert_eq!(
        tool_round_limit_status(25, Some(50)),
        "tool-call round limit: 50 this session (config/model default 25)"
    );
    assert!(
        tool_round_limit_status(25, Some(EFFECTIVELY_UNLIMITED_TOOL_ROUNDS))
            .contains("effectively unlimited")
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
    assert!(parse_spill_command("/spillage 7").is_err());
    assert!(parse_spill_command("/spill many").is_err());
}

#[test]
fn spill_override_resolves_and_reports_live_capability() {
    assert_eq!(effective_spill_lines(3, None), 3);
    assert_eq!(effective_spill_lines(3, Some(7)), 7);
    assert_eq!(effective_spill_lines(3, Some(0)), 0);
    assert_eq!(
        spill_status(3, None, true),
        "spill rows: 3 (config default; live interaction available)"
    );
    assert_eq!(
        spill_status(3, Some(7), false),
        "spill rows: 7 this session (config default 3; live interaction unavailable)"
    );
    assert_eq!(
        spill_status(3, Some(0), true),
        "spill rows: unbounded this session (config default 3; live viewport disabled)"
    );
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
    assert!(dispatch_slash("/help", "/ws", false, false).unwrap());
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
        "loadout",
        "workspace",
        "spill",
        "config",
        "prompt",
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
    let spill_help = command_help_page("spill").expect("spill help page");
    assert!(spill_help.contains("Space or Enter"));
    assert!(spill_help.contains("⧉"));
    assert!(spill_help.contains("▣"));
    // The unknown-topic miss is reported (returns false).
    assert!(!print_command_help("bogus", false, false));
}

/// ANTI-DRIFT: the startup-free `render_help` path (behind `newt help`) must
/// emit bytes IDENTICAL to what the interactive REPL prints for `/help` and
/// `/<cmd> --help`. Both surfaces route through `render_help`, so this pins the
/// output against the single-source-of-truth corpora (`help_lines` /
/// `command_help_page`) reconstructed the way the REPL printed them before the
/// decouple: `print_newt` == `println!(newt_line(...))` (a `newt_line` + `\n`),
/// then one corpus line per `\n`-terminated row. If `render_help` ever reshapes,
/// drops, or reorders a line relative to the corpus, this fails — the exact
/// plausible-but-unwired divergence a forked second help path would introduce.
#[test]
fn render_help_is_byte_identical_to_the_repl_help_corpus() {
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

#[test]
fn slash_version_returns_true() {
    assert!(dispatch_slash("/version", "/ws", false, false).unwrap());
}

#[test]
fn slash_workspace_returns_true() {
    assert!(dispatch_slash("/workspace", "/ws", false, false).unwrap());
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
    assert!(dispatch_slash("/config", "/ws", false, false).unwrap());
}

#[test]
fn help_lists_config_command() {
    assert!(help_lines().iter().any(|l| l.contains("/config")));
}

#[test]
fn slash_unknown_returns_true() {
    assert!(dispatch_slash("/notacommand", "/ws", false, false).unwrap());
}

#[test]
fn slash_dgx_no_subcmd_returns_true() {
    assert!(dispatch_slash("/dgx", "/ws", false, false).unwrap());
}
