//! #2932: one sweep over every harness guidance producer. Each producer takes
//! the resolved operator policy and must never name a tool that policy hides,
//! while keeping its tool-free advice. A new producer is added here, not to a
//! per-name test, so the next hidden-name leak is caught by construction.
//!
//! Workflow-pack `steer` prose and the operator's classifier directions are
//! data the harness does not rewrite; the sweep drives the pack renderer with
//! a synthetic pack whose steps list every catalog tool, so the harness-owned
//! prefix, suffix and `(tools: …)` clause are covered without English
//! vocabulary ("find", "search") standing in for a recipe.
use super::*;
use crate::agentic::tools::ExposureSettings;
use scheduled::{SessionStepLedger, StepLedger};

/// Every tool name the harness can advertise with everything switched on.
fn catalog_names() -> Vec<String> {
    merged_tool_definitions(
        &NoMcp,
        true,
        true,
        true,
        Some(&crate::caveats::Scope::All),
        true,
        true,
        true,
        true,
        true,
        true,
        true,
        true,
        Default::default(),
        crate::agentic::shell_dialect_sentence(false, false),
    )
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|tool| tool["function"]["name"].as_str().map(String::from))
    .collect()
}

/// Whether `text` contains `name` as a whole identifier token.
fn names_tool(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token == name)
}

/// A pack whose one step suggests every catalog tool, with tool-free prose.
fn synthetic_pack(catalog: &[String]) -> crate::WorkflowSteerer {
    let tools = catalog
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let config: crate::WorkflowConfig = toml::from_str(&format!(
        r#"
name = "sweep"
description = "A synthetic pack for the guidance sweep"

[classifier]
keywords = ["sweep the guidance"]

[[steps]]
id = "every_tool"
title = "A step that suggests every tool"
steer = "Do the step"
tools = [{tools}]
"#
    ))
    .expect("synthetic pack parses");
    crate::WorkflowSteerer::from_workflows(vec![config])
}

/// Every guidance text the harness can hand the model under `policy`.
fn guidance_under(policy: &ExposureSettings, catalog: &[String]) -> Vec<(&'static str, String)> {
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&[
        "read the module".into(),
        "fix the bug".into(),
        "verify".into(),
    ]);
    ledger.advance();
    let ledger: &dyn StepLedger = &ledger;
    let task = "fix the failing test";
    let turn = crate::TurnPromptContext::ephemeral_operator("conv", task, task);
    let with_receipt = prompt_read::PromptReadContext::new(Some(&turn), task, None);
    let without_receipt = prompt_read::PromptReadContext::new(None, task, None);
    let pack_hint = synthetic_pack(catalog)
        .plan_update_hint("sweep the guidance", policy)
        .expect("synthetic pack matches");
    let checks = self_verify::detect_checks(&["Cargo.toml".to_string()], "");
    let mut out = vec![
        (
            "workflow_guidance",
            workflow_guidance("do the next step", policy),
        ),
        (
            "permission_recovery_guidance",
            permission_recovery_guidance(policy),
        ),
        (
            "post_compaction_continuation (receipt, plan)",
            post_compaction_continuation(None, Some(ledger), with_receipt, policy),
        ),
        (
            "post_compaction_continuation (no receipt)",
            post_compaction_continuation(None, None, without_receipt, policy),
        ),
        ("narration_action_nudge", narration_action_nudge(policy)),
        (
            "escalated_narration_action_nudge",
            escalated_narration_action_nudge(2, 3, Some(ledger), policy),
        ),
        (
            "read_only_action_nudge",
            read_only_action_nudge(3, Some(2), Some(ledger), None, policy),
        ),
        (
            "pending_plan_completion_nudge (plan update, pack hint)",
            pending_plan_completion_nudge(Some(ledger), true, Some(&pack_hint), policy).unwrap(),
        ),
        (
            "pending_plan_completion_nudge",
            pending_plan_completion_nudge(Some(ledger), false, None, policy).unwrap(),
        ),
        (
            "stale_file_ground_truth_nudge",
            stale_file_ground_truth_nudge(policy),
        ),
        (
            "plan_reseat_pointer",
            plan_reseat_pointer(ledger, policy).expect("multi-step plan"),
        ),
        ("plan_update_hint", pack_hint),
        (
            "active_prompt_card",
            prompt_read::active_prompt_card(
                prompt_read::PromptReadContext::new(Some(&turn), task, None),
                policy,
            ),
        ),
        (
            "output_budget::guidance",
            output_budget::guidance(
                Some(400),
                Some(ledger),
                std::path::Path::new("unused"),
                &crate::caveats::Scope::All,
                crate::tokens::TokenEstimation::default(),
                policy,
            ),
        ),
    ];
    if let Some(gate) = self_verify::verify_gate_nudge(&checks, &[], policy) {
        out.push(("verify_gate_nudge", gate));
    }
    out
}

/// Under the pi-style four-tool list, under `read_only`, and under an empty
/// allow list, no producer names a hidden tool; the tool-free advice stays.
#[test]
fn no_guidance_producer_names_an_operator_hidden_tool() {
    let four_tools = ExposureSettings {
        allow_list: Some(
            ["read_file", "run_command", "edit_file", "write_file"]
                .map(String::from)
                .to_vec(),
        ),
        ..Default::default()
    };
    let read_only = ExposureSettings {
        read_only: true,
        ..Default::default()
    };
    let nothing = ExposureSettings {
        allow_list: Some(Vec::new()),
        ..Default::default()
    };
    let catalog = catalog_names();
    assert!(catalog.len() > 20, "{catalog:?}");
    for (label, policy) in [
        ("four tools", &four_tools),
        ("read_only", &read_only),
        ("empty allow list", &nothing),
    ] {
        // `git` is both a tool and the command word a run_command probe spells
        // (`git status --short`); a shell recipe under a visible run_command is
        // a run_command recipe, not a git-tool recipe.
        let hidden: Vec<&str> = catalog
            .iter()
            .map(String::as_str)
            .filter(|name| !policy.operator_visible(name))
            .filter(|name| !(*name == "git" && policy.operator_visible("run_command")))
            .collect();
        assert!(!hidden.is_empty(), "{label}: policy hides nothing");
        let mut leaks = Vec::new();
        for (producer, text) in guidance_under(policy, &catalog) {
            assert!(
                !text.trim().is_empty(),
                "{label}: {producer} rendered nothing"
            );
            for name in &hidden {
                if names_tool(&text, name) {
                    leaks.push(format!(
                        "{label}: {producer} names hidden `{name}`:\n{text}"
                    ));
                }
            }
        }
        assert!(leaks.is_empty(), "{}", leaks.join("\n\n"));
    }
    // The twin: with no policy the same producers do name the tools, so the
    // sweep above is filtering something real. The synthetic pack's step
    // suggests the whole catalog, so every name must surface at least there.
    let open = ExposureSettings::default();
    let all: String = guidance_under(&open, &catalog)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    for name in catalog.iter().map(String::as_str).chain([
        "update_plan",
        "request_permissions",
        "prompt_read",
        "artifact_read",
    ]) {
        assert!(names_tool(&all, name), "open policy should name {name}");
    }
}
