use super::*;

// Remaining session controls in Config/TuiConfig and per-model runtime tuning.

#[test]
fn scratch_section_defaults_and_parses() {
    // #844: `[scratch] dir` parses onto Config; absent → None (the `.scratch`
    // default applies at resolution). Uses `from_str` (not `resolve`) so this
    // does NOT publish a process-global scratch dir.
    let bare: Config = toml::from_str("").unwrap();
    assert!(bare.scratch.is_none());
    let cfg: Config = toml::from_str("[scratch]\ndir = \"/tmp/newt-scratch\"\n").unwrap();
    assert_eq!(
        cfg.scratch.and_then(|s| s.dir).as_deref(),
        Some("/tmp/newt-scratch")
    );
}

#[test]
fn allow_bang_escape_defaults_to_true_and_round_trips() {
    // Absent key → enabled (the human's host shell-out is on by default).
    let cfg: TuiConfig = toml::from_str("").unwrap();
    assert!(cfg.allow_bang_escape);
    // Explicit opt-out parses.
    let cfg: TuiConfig = toml::from_str("allow_bang_escape = false").unwrap();
    assert!(!cfg.allow_bang_escape);
}

#[test]
fn shell_commands_default_on_mutations_default_off_and_round_trip() {
    // Navigation/inspection suite on by default; mutations off until opted in.
    let cfg: TuiConfig = toml::from_str("").unwrap();
    assert!(cfg.allow_shell_commands);
    assert!(!cfg.allow_shell_mutations);
    let cfg: TuiConfig =
        toml::from_str("allow_shell_commands = false\nallow_shell_mutations = true").unwrap();
    assert!(!cfg.allow_shell_commands);
    assert!(cfg.allow_shell_mutations);
}

#[test]
fn conversations_config_defaults_to_count_cap() {
    let cfg = Config::default();
    let conversations = cfg.conversations.unwrap_or_default();
    assert_eq!(conversations.max_per_workspace, 100);
    // #1030: fresh-on-launch — auto-resume defaults OFF now; `resume = true`
    // is the opt-in back to auto-resuming the folder's latest conversation.
    assert!(!conversations.resume);
}

#[test]
fn conversations_config_roundtrips_through_toml() {
    let cfg: Config = toml::from_str(
        r#"
[conversations]
max_per_workspace = 25
"#,
    )
    .unwrap();

    let conversations = cfg.conversations.unwrap_or_default();
    assert_eq!(conversations.max_per_workspace, 25);
    // Partial [conversations] table: unset keys keep their defaults
    // (#1030: `resume` now defaults false = fresh-on-launch).
    assert!(!conversations.resume);
}

#[test]
fn conversations_resume_opt_in_parses() {
    // #1030: `resume = true` opts back into auto-resuming the folder's
    // latest conversation (the pre-#1030 default, now off by default).
    let cfg: Config = toml::from_str(
        r#"
[conversations]
resume = true
"#,
    )
    .unwrap();

    assert!(cfg.conversations.unwrap_or_default().resume);
}

#[test]
fn agents_config_default_enabled() {
    let cfg = AgentsConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.path, None);
    // A bare Config defaults agents to enabled too.
    assert!(Config::default().agents.enabled);
}

#[test]
fn agents_config_roundtrips_with_path() {
    let cfg: Config = toml::from_str(
        r#"
[agents]
path = "docs/instructions"
"#,
    )
    .unwrap();
    assert!(cfg.agents.enabled);
    assert_eq!(cfg.agents.path.as_deref(), Some("docs/instructions"));

    // Serialize back out and confirm the path survives.
    let text = toml::to_string(&cfg).unwrap();
    assert!(text.contains("docs/instructions"));
}

#[test]
fn agents_config_can_be_disabled() {
    let cfg: Config = toml::from_str(
        r#"
[agents]
enabled = false
"#,
    )
    .unwrap();
    assert!(!cfg.agents.enabled);
    assert_eq!(cfg.agents.path, None);
}

#[test]
fn default_max_tool_rounds_is_40() {
    // #<issue>: raised from 25 — a modest safety margin alongside
    // workflow_grace_rounds and the diagnose_failure delegate hint, not a
    // substitute for either. The function default and the struct default
    // agree on 40.
    assert_eq!(default_max_tool_rounds(), 40);
    assert_eq!(TuiConfig::default().max_tool_rounds, 40);
    assert_eq!(default_workflow_grace_rounds(), 5);
    assert_eq!(TuiConfig::default().workflow_grace_rounds, 5);
}

#[test]
fn tui_max_tool_rounds_defaults_when_field_absent() {
    // An empty `[tui]` table => serde default kicks in => 40.
    let toml = r#"
            [tui]
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.tui.unwrap().max_tool_rounds, 40);
}

#[test]
fn tui_max_tool_rounds_can_be_overridden() {
    let toml = r#"
            [tui]
            max_tool_rounds = 7
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.tui.unwrap().max_tool_rounds, 7);
}

#[test]
fn tui_narration_nudge_cap_defaults_to_one_and_can_be_raised() {
    // Lever L3 (next-loop-levers.md): the narrate-then-stop rescue budget
    // is config, not a hardcoded const. Default 1 preserves the historical
    // behavior; the function default and the struct default agree.
    assert_eq!(default_narration_nudge_cap(), 1);
    assert_eq!(TuiConfig::default().narration_nudge_cap, 1);

    // An empty `[tui]` table => serde default kicks in => 1.
    let cfg: Config = toml::from_str("[tui]\n").unwrap();
    assert_eq!(cfg.tui.unwrap().narration_nudge_cap, 1);

    // Weak-local-model operators raise it.
    let cfg: Config = toml::from_str("[tui]\nnarration_nudge_cap = 3\n").unwrap();
    assert_eq!(cfg.tui.unwrap().narration_nudge_cap, 3);
}

#[test]
fn model_tuning_narration_nudge_cap_override_parses() {
    let cfg: Config = toml::from_str(
        r#"
            [[model_tuning]]
            model = "ornith:35b"
            narration_nudge_cap = 3
        "#,
    )
    .unwrap();
    let tune = cfg.find_model_tuning("ornith:35b").unwrap();
    assert_eq!(tune.narration_nudge_cap, Some(3));
    // Absent field stays None (inherit the [tui] value).
    let cfg: Config = toml::from_str(
        r#"
            [[model_tuning]]
            model = "other:7b"
            max_tool_rounds = 9
        "#,
    )
    .unwrap();
    assert_eq!(
        cfg.find_model_tuning("other:7b")
            .unwrap()
            .narration_nudge_cap,
        None
    );
}

#[test]
fn tui_workflow_grace_rounds_can_be_overridden_or_disabled() {
    let cfg: Config = toml::from_str(
        r#"
            [tui]
            workflow_grace_rounds = 9
        "#,
    )
    .unwrap();
    assert_eq!(cfg.tui.unwrap().workflow_grace_rounds, 9);

    let disabled: Config = toml::from_str(
        r#"
            [tui]
            workflow_grace_rounds = 0
        "#,
    )
    .unwrap();
    assert_eq!(disabled.tui.unwrap().workflow_grace_rounds, 0);
}

#[test]
fn model_tuning_parses_from_toml() {
    let toml = r#"
            [[model_tuning]]
            model = "nemotron3:33b"
            num_ctx = 24576
            mid_loop_trim_threshold = 12
            max_tool_rounds = 20
            workflow_grace_rounds = 8

            [[model_tuning]]
            model = "qwen3-coder:30b"
            num_ctx = 65536
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.model_tuning.len(), 2);

    let nemo = cfg.find_model_tuning("nemotron3:33b").unwrap();
    assert_eq!(nemo.num_ctx, Some(24576));
    assert_eq!(nemo.mid_loop_trim_threshold, Some(12));
    assert_eq!(nemo.max_tool_rounds, Some(20));
    assert_eq!(nemo.workflow_grace_rounds, Some(8));

    let qwen = cfg.find_model_tuning("qwen3-coder:30b").unwrap();
    assert_eq!(qwen.num_ctx, Some(65536));
    assert_eq!(qwen.mid_loop_trim_threshold, None);
    assert_eq!(qwen.workflow_grace_rounds, None);
}

#[test]
fn model_tuning_find_returns_none_for_unknown_model() {
    let cfg = Config::default();
    assert!(cfg.find_model_tuning("nonexistent:7b").is_none());
}

#[test]
fn model_tuning_partial_fields_are_optional() {
    let toml = r#"
            [[model_tuning]]
            model = "llama3.1:8b"
        "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let entry = cfg.find_model_tuning("llama3.1:8b").unwrap();
    assert_eq!(entry.num_ctx, None);
    assert_eq!(entry.mid_loop_trim_threshold, None);
    assert_eq!(entry.max_tool_rounds, None);
    assert_eq!(entry.workflow_grace_rounds, None);
}
