use super::*;
use newt_core::agentic::PromptDisposition;

#[test]
fn operating_modes_have_the_canonical_names_and_human_descriptions() {
    let names = OperatingMode::all()
        .iter()
        .map(|mode| mode.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "chat",
            "dev",
            "admin",
            "plan",
            "diagnose",
            "auto",
            "full-auto"
        ]
    );
    for mode in OperatingMode::all() {
        assert!(
            !mode.description().trim().is_empty(),
            "{} needs a human-readable description",
            mode.as_str()
        );
    }
}

#[test]
fn operating_mode_parser_accepts_canonical_names_and_safe_aliases() {
    assert_eq!(
        OperatingMode::from_keyword("chat"),
        Some(OperatingMode::Chat)
    );
    assert_eq!(
        OperatingMode::from_keyword("developer"),
        Some(OperatingMode::Dev)
    );
    assert_eq!(
        OperatingMode::from_keyword("sysadmin"),
        Some(OperatingMode::Admin)
    );
    assert_eq!(
        OperatingMode::from_keyword("diagnostic"),
        Some(OperatingMode::Diagnose)
    );
    assert_eq!(
        OperatingMode::from_keyword("full_auto"),
        Some(OperatingMode::FullAuto)
    );
    assert_eq!(OperatingMode::from_keyword("unrestricted"), None);
}

#[test]
fn mode_command_lists_describes_sets_and_resets_without_invalid_mutation() {
    let mut active = OperatingMode::Chat;
    let listing = operating_mode_command_lines("", &mut active).unwrap();
    for mode in OperatingMode::all() {
        assert!(
            listing
                .iter()
                .any(|line| line.contains(mode.as_str()) && line.contains(mode.description())),
            "missing {} from {listing:?}",
            mode.as_str()
        );
    }

    let changed = operating_mode_command_lines("dev", &mut active).unwrap();
    assert_eq!(active, OperatingMode::Dev);
    assert!(changed.join("\n").contains("operating mode set to dev"));

    let err = operating_mode_command_lines("god-mode", &mut active).unwrap_err();
    assert!(err.contains("unknown /mode"));
    assert_eq!(
        active,
        OperatingMode::Dev,
        "invalid input must not change the active mode"
    );

    operating_mode_command_lines("reset", &mut active).unwrap();
    assert_eq!(active, OperatingMode::Chat);
}

#[test]
fn plan_and_diagnose_share_one_effective_disposition_with_the_executor() {
    let mut plan = newt_core::agentic::PromptIntake::analyze("Implement the parser change.");
    apply_operating_mode_to_intake(OperatingMode::Plan, &mut plan);
    assert_eq!(plan.disposition(), PromptDisposition::Plan);
    assert!(plan.model_card().contains("disposition: plan"));
    assert!(newt_core::agentic::tool_allowed(
        plan.disposition(),
        "update_plan"
    ));
    assert!(!newt_core::agentic::tool_allowed(
        plan.disposition(),
        "write_file"
    ));

    let mut diagnose = newt_core::agentic::PromptIntake::analyze("Implement the parser change.");
    apply_operating_mode_to_intake(OperatingMode::Diagnose, &mut diagnose);
    assert_eq!(diagnose.disposition(), PromptDisposition::Research);
    assert!(diagnose.model_card().contains("disposition: research"));
    assert!(!newt_core::agentic::tool_allowed(
        diagnose.disposition(),
        "update_plan"
    ));

    let mut ask = newt_core::agentic::PromptIntake::analyze("Delete it.");
    apply_operating_mode_to_intake(OperatingMode::Plan, &mut ask);
    assert_eq!(
        ask.disposition(),
        PromptDisposition::Ask,
        "mode must not bypass a pending human decision"
    );

    let mut research = newt_core::agentic::PromptIntake::analyze("Investigate the parser.");
    apply_operating_mode_to_intake(OperatingMode::Dev, &mut research);
    assert_eq!(
        research.disposition(),
        PromptDisposition::Research,
        "dev must not widen a read-only intake disposition"
    );
}

#[test]
fn explicit_action_modes_render_disposition_compatible_instructions() {
    let cases = [
        (
            "Investigate the parser regression.",
            PromptDisposition::Research,
            OperatingMode::Diagnose,
        ),
        (
            "Explain how the parser works.",
            PromptDisposition::Explain,
            OperatingMode::Chat,
        ),
    ];
    for configured in [
        OperatingMode::Dev,
        OperatingMode::Admin,
        OperatingMode::FullAuto,
    ] {
        for (prompt, disposition, expected) in cases {
            let intake = newt_core::agentic::PromptIntake::analyze(prompt);
            assert_eq!(intake.disposition(), disposition, "{prompt}");
            let effective = effective_operating_mode(configured, &intake, false, None);
            assert_eq!(effective, expected, "{configured:?}: {prompt}");
            let rendered = operating_mode_prompt(configured, effective);
            assert!(
                rendered.contains(&format!("effective=\"{}\"", expected.as_str())),
                "{rendered}"
            );
            if configured == OperatingMode::Admin {
                assert!(rendered.contains("Do no harm"), "{rendered}");
                assert!(rendered.contains("Respect privacy"), "{rendered}");
            }
        }

        let mut plan_intake = newt_core::agentic::PromptIntake::analyze("Repair the parser.");
        plan_intake.enforce_read_only(PromptDisposition::Plan);
        assert_eq!(
            effective_operating_mode(configured, &plan_intake, false, None),
            OperatingMode::Plan,
            "{configured:?} must render Plan-compatible instructions for protected Plan intake"
        );
    }
}

#[test]
fn auto_selects_a_bounded_effective_mode_per_turn_and_never_full_auto() {
    let cases = [
        ("Implement the parser change.", OperatingMode::Dev),
        (
            "Investigate the parser regression.",
            OperatingMode::Diagnose,
        ),
        ("Explain the parser.", OperatingMode::Chat),
        ("Write a plan for the parser repair.", OperatingMode::Plan),
        ("Use admin mode for this server task.", OperatingMode::Admin),
        ("Implement plan mode support.", OperatingMode::Dev),
        ("Fix the diagnose mode bug.", OperatingMode::Dev),
        ("Explain plan mode.", OperatingMode::Chat),
    ];
    for (prompt, expected) in cases {
        let intake = newt_core::agentic::PromptIntake::analyze(prompt);
        let effective = effective_operating_mode(OperatingMode::Auto, &intake, false, None);
        assert_eq!(effective, expected, "{prompt}");
        assert_ne!(effective, OperatingMode::FullAuto, "{prompt}");
    }

    let intake = newt_core::agentic::PromptIntake::analyze("Implement the parser change.");
    assert_eq!(
        effective_operating_mode(OperatingMode::Auto, &intake, true, Some(OperatingMode::Dev)),
        OperatingMode::Plan,
        "a model-entered plan phase must be visible in the effective mode"
    );
}

#[test]
fn auto_model_selection_applies_only_to_action_turns_and_one_conversation() {
    let state = AutoModeState::default();
    let control = state.bind("conversation-a");
    let result =
        newt_core::agentic::OperatingModeControl::select_operating_mode(&control, "admin").unwrap();
    assert!(result.contains("next action-shaped turn"));
    assert!(result.contains("current turn"));

    let research = newt_core::agentic::PromptIntake::analyze("Investigate the parser.");
    assert_eq!(
        effective_operating_mode(OperatingMode::Auto, &research, false, None,),
        OperatingMode::Diagnose,
        "protected intake wins without consuming a stored action style"
    );
    assert_eq!(
        state.pending_for("conversation-a"),
        Some(OperatingMode::Admin)
    );

    let action = newt_core::agentic::PromptIntake::analyze("Implement the parser change.");
    assert_eq!(
        effective_operating_mode(
            OperatingMode::Auto,
            &action,
            false,
            state.take_for("conversation-a"),
        ),
        OperatingMode::Admin
    );
    assert_eq!(
        state.pending_for("conversation-a"),
        None,
        "the model-selected style is consumed by one action turn"
    );
    assert_eq!(
        effective_operating_mode(
            OperatingMode::Auto,
            &action,
            false,
            state.take_for("conversation-a"),
        ),
        OperatingMode::Dev,
        "later action turns return to deterministic Auto selection"
    );
}

#[test]
fn auto_model_selection_rejects_self_escalation() {
    let state = AutoModeState::default();
    let control = state.bind("conversation-a");
    for mode in ["auto", "full-auto", "unknown"] {
        let error = newt_core::agentic::OperatingModeControl::select_operating_mode(&control, mode)
            .unwrap_err();
        assert!(
            error.contains("cannot be model-selected") || error.contains("choose one of"),
            "{mode}: {error}"
        );
    }
    assert_eq!(state.pending_for("conversation-a"), None);
}

#[test]
fn plan_and_diagnose_attenuate_caveats_while_full_auto_preserves_them() {
    use newt_core::CaveatsExt as _;

    let base = newt_core::Caveats::top();
    for mode in [OperatingMode::Plan, OperatingMode::Diagnose] {
        let effective = operating_mode_caveats(mode, base.clone());
        assert!(effective.leq(&base), "{mode:?} must only attenuate");
        assert!(effective.permits_fs_read("/workspace/src/lib.rs"));
        assert!(!effective.permits_fs_write("/workspace/src/lib.rs"));
        assert!(!effective.permits_exec("cargo"));
    }
    assert!(
        !operating_mode_caveats(OperatingMode::Plan, base.clone()).permits_net("example.com"),
        "Plan remains offline"
    );
    assert!(
        operating_mode_caveats(OperatingMode::Diagnose, base.clone()).permits_net("example.com"),
        "Diagnose may gather remote read-only evidence"
    );
    assert_eq!(
        operating_mode_caveats(OperatingMode::FullAuto, base.clone()),
        base,
        "full-auto changes persistence, not authority"
    );
}

#[test]
fn mode_instructions_pin_the_human_requested_safety_contracts() {
    let dev = OperatingMode::Dev.instructions();
    assert!(dev.contains("TDD") && dev.contains("worktree") && dev.contains("full preflight"));

    let full_auto = OperatingMode::FullAuto.instructions();
    assert!(
        full_auto.contains("TDD")
            && full_auto.contains("worktree")
            && full_auto.contains("full preflight")
    );

    let admin = OperatingMode::Admin.instructions();
    assert!(admin.contains("Do no harm"));
    assert!(admin.contains("Make minimal changes"));
    assert!(admin.contains("Respect privacy"));
    assert!(admin.contains("With great power comes great responsibility"));

    let diagnose = OperatingMode::Diagnose.instructions();
    assert!(diagnose.contains("Seek only to understand"));
    assert!(diagnose.contains("switch to /mode plan"));

    let auto = OperatingMode::Auto.instructions();
    assert!(auto.contains("effective style"));
    assert!(auto.contains("Ask the human"));
    assert!(auto.contains("never selects full-auto"));
}

#[test]
fn explicit_mode_selection_clears_legacy_plan_phase_but_show_does_not() {
    let mut active = OperatingMode::Plan;
    let mode_states = ConversationModeStates::default();
    let control = mode_states.auto.bind("conversation-a");
    newt_core::agentic::OperatingModeControl::select_operating_mode(&control, "admin").unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();

    handle_operating_mode_command("show", &mut active, &mode_states, false, false);
    assert!(mode_states.plan.is_active());
    assert_eq!(
        mode_states.auto.pending_for("conversation-a"),
        Some(OperatingMode::Admin)
    );

    handle_operating_mode_command("dev", &mut active, &mode_states, false, false);
    assert_eq!(active, OperatingMode::Dev);
    assert!(
        !mode_states.plan.is_active(),
        "the human's explicit mode must supersede stale model plan state"
    );
    assert_eq!(
        mode_states.auto.pending_for("conversation-a"),
        None,
        "the human's explicit mode must supersede model-selected Auto state"
    );
}

#[test]
fn conversation_boundary_clears_plan_and_auto_state_without_resurrection() {
    let mode_states = ConversationModeStates::default();

    let a = mode_states.auto.bind("conversation-a");
    newt_core::agentic::OperatingModeControl::select_operating_mode(&a, "admin").unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();
    mode_states.clear();
    assert_eq!(mode_states.auto.pending_for("conversation-a"), None);
    assert!(!mode_states.plan.is_active());

    let b = mode_states.auto.bind("conversation-b");
    newt_core::agentic::OperatingModeControl::select_operating_mode(&b, "dev").unwrap();
    newt_core::agentic::PlanModeControl::set_plan_mode(&mode_states.plan, true).unwrap();
    mode_states.clear();
    assert_eq!(
        mode_states.auto.pending_for("conversation-b"),
        None,
        "A→B→A boundary sequence must not resurrect B's pending Auto selection"
    );
    assert_eq!(mode_states.auto.pending_for("conversation-a"), None);
    assert!(!mode_states.plan.is_active());
}

#[test]
fn live_session_control_prompt_composes_mode_and_posture_without_stale_state() {
    let posture = ActivePosture {
        name: "triage".to_string(),
        preset_name: "readonly-triage".to_string(),
        clamp: newt_core::Caveats::top(),
        clamp_summary: "readonly".to_string(),
        skill_body: Some("Inspect evidence before drawing conclusions.".to_string()),
        framing: Some("Treat this as an on-call incident.".to_string()),
    };
    let active = session_control_prompt(
        OperatingMode::Diagnose,
        OperatingMode::Diagnose,
        Some(&posture),
    );
    assert!(active.contains("Operating mode: diagnose"), "{active}");
    assert!(
        active.contains("Active permission posture: triage"),
        "{active}"
    );
    assert!(active.contains("Inspect evidence"), "{active}");
    assert!(active.contains("on-call incident"), "{active}");

    let cleared = session_control_prompt(OperatingMode::Chat, OperatingMode::Chat, None);
    assert!(cleared.contains("Operating mode: chat"), "{cleared}");
    assert!(!cleared.contains("triage"), "{cleared}");
    assert!(!cleared.contains("Inspect evidence"), "{cleared}");

    let auto = session_control_prompt(OperatingMode::Auto, OperatingMode::Dev, None);
    assert!(auto.contains("Configured session mode: auto"), "{auto}");
    assert!(
        auto.contains("Effective working style for this turn: dev"),
        "{auto}"
    );
    assert!(auto.contains("select_operating_mode"), "{auto}");
    assert!(
        auto.contains(OperatingMode::Dev.instructions()),
        "effective instructions must be rendered: {auto}"
    );
    assert!(
        !auto.contains(OperatingMode::Auto.instructions()),
        "configured metadata must not emit conflicting behavioral instructions: {auto}"
    );

    let overridden = session_control_prompt(OperatingMode::Dev, OperatingMode::Plan, None);
    assert!(overridden.contains(OperatingMode::Plan.instructions()));
    assert!(
        !overridden.contains(OperatingMode::Dev.instructions()),
        "legacy Plan must not be paired with conflicting Dev instructions: {overridden}"
    );
}
