//! Capability-claim verification for reports (#1947) and fresh probes (#2454).
//!
//! [`claim_check`](super::claim_check) verifies *path* claims and appends a
//! visible refutation for anything that does not resolve. This is its sibling
//! for *capability* claims — "verified", "working end-to-end", "tested", a
//! table of green checkmarks.
//!
//! The #1947 session: `render_report` shipped a ten-row all-✅ table and the
//! line "MCP stdio transport verified working end-to-end", off ONE
//! `tools/call` for `list_audio_devices` that touched none of the voice path.
//! STT, TTS, VAD and `converse` were never invoked, `cargo test` was never
//! run, and every daemon run in the log died on an espeak error. **Every path
//! newt cited existed, so `claim_check` was satisfied.** The claim that was
//! false was behavioral.
//!
//! # What counts as evidence
//!
//! The turn's [`ToolEvent`](crate::ToolEvent) ledger, and only that. It
//! records `{tool, args_digest, ok, execution}` — and `args_digest` is deliberately
//! **key names plus a hash, never values**, because args carry file contents
//! and secrets. So "was `cargo test` run?" is *not* answerable here: the
//! ledger knows `run_command` was called with a `command` key and its typed
//! execution outcome, but not which command ran. Designing against what the ledger can actually prove rather than
//! what would be convenient is the whole discipline; a check that pretended
//! to know the command would be the false claim it exists to catch.
//!
//! Reports accept arbitrary Markdown. A table label such as `cargo build`
//! or `Guard tests` is not a tool identity, and rendering it is not an
//! attestation. This ledger cannot verify individual report claims by matching
//! those labels to tool names.
//!
//! It can establish whether anything ran, whether a fresh execution probe has
//! a returned execution outcome, and how many calls returned failure during
//! the turn. Past failures remain historical observations: a later success
//! from the same tool may be an entirely different command, while an earlier
//! failure does not prove that the reported final result is still failing.
//!
//! Preserve the model's document exactly and append only those bounded facts.
//! No annotation means no contradiction was established, not independent
//! verification of the report. Pure by construction: evidence is a value
//! distilled from the ledger, so the unit tier stays fully mocked.

use crate::ToolEvent;

/// What the turn actually did, distilled from the ledger.
///
/// Deliberately tiny and OWNED. The alternative — lending the live
/// `Vec<ToolEvent>` down to the tool-execution site — would fight the
/// borrow the loop already holds to push into it, for no gain: nothing here
/// needs more than the names and the outcomes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// Tool names invoked this turn, first-seen order, deduplicated.
    invoked: Vec<String>,
    /// Calls whose result read as success.
    ok: usize,
    /// Calls whose result read as failure.
    failed: usize,
    /// A returned execution attempt (including denial/unavailability), or a
    /// Git capability query. History and catalog reads are not execution probes.
    execution_observed: bool,
}

impl Evidence {
    /// Distil the ledger. Records every call, successful or not: a failed
    /// call is evidence too — it is how "every daemon run died on an espeak
    /// error" becomes checkable.
    pub(crate) fn from_events(events: &[ToolEvent]) -> Self {
        let mut out = Self::default();
        for event in events {
            out.record(&event.tool, event.ok, event.execution);
        }
        out
    }

    /// Fed at the same completed-dispatch seam as ToolEvent, even when the
    /// caller did not request persisted telemetry. Never inferred from prose.
    pub(crate) fn record(&mut self, tool: &str, ok: bool, execution: Option<crate::ExecOutcome>) {
        if !self.invoked.iter().any(|name| name == tool) {
            self.invoked.push(tool.to_string());
        }
        if ok {
            self.ok += 1;
        } else {
            self.failed += 1;
        }
        self.execution_observed |= execution.is_some() || tool == "git";
    }

    pub(crate) fn unsupported_probe_claim(&self, text: &str) -> bool {
        !self.execution_observed
            && claims_fresh_execution_probe(
                text,
                self.invoked
                    .iter()
                    .any(|tool| tool == "request_permissions"),
            )
    }

    pub(crate) fn correction(
        &self,
        text: &str,
        tools: &serde_json::Value,
        used: &mut bool,
        allowed: bool,
    ) -> Option<String> {
        if !allowed || *used || !self.unsupported_probe_claim(text) {
            return None;
        }
        let nudge = probe_correction(tools)?;
        *used = true;
        Some(nudge)
    }

    /// Nothing ran at all. The strongest refutation available: a report
    /// claiming verification over a turn that invoked no tool has nothing
    /// behind it whatsoever.
    pub(crate) fn is_silent(&self) -> bool {
        self.invoked.is_empty()
    }
}

/// Deliberately narrow: completed execution probes, including negative results.
/// This is not a truth classifier for arbitrary prose. Quoted examples, saved
/// history, future intent and explicit nonverification must remain ordinary text.
fn claims_fresh_execution_probe(text: &str, permission_requested: bool) -> bool {
    let lower = text.to_lowercase().replace('’', "'");
    if ![
        "cargo",
        "rustc",
        "git",
        "toolchain",
        "shell",
        "command",
        "executable",
        "lifecycle",
    ]
    .iter()
    .any(|name| lower.contains(name))
    {
        return false;
    }
    let mut fenced = false;
    lower.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            return false;
        }
        if fenced
            || line.starts_with(['>', '"'])
            || line
                .trim_start_matches(['-', '*', '•', ' '])
                .starts_with("if ")
            || [
                "not re-probed",
                "not reprobed",
                "haven't re-probed",
                "haven't reprobed",
                "not probed",
                "haven't probed",
                "not checked",
                "haven't checked",
                "not attempted",
                "haven't attempted",
                "before execution",
                "no command ran",
                "would be denied",
                "might be denied",
                "could be denied",
                "did not",
                "didn't",
                "previous",
                "earlier",
                "last session",
                "saved state",
                "history says",
                "you said",
                "user said",
                "for example",
                "example:",
            ]
            .iter()
            .any(|phrase| line.contains(phrase))
        {
            return false;
        }
        [
            "probes came back",
            "probes reported",
            "re-probed directly",
            "reprobed directly",
            "direct re-probe returned",
            "direct reprobe returned",
            "i ran the probes",
            "i checked again",
            "i've checked again",
            "i re-probed",
            "i reprobed",
        ]
        .iter()
        .any(|phrase| line.contains(phrase))
            || ["i've attempted ", "i have attempted ", "i attempted "]
                .iter()
                .filter_map(|phrase| line.split_once(phrase))
                .any(|(_, subject)| {
                    ["cargo", "rustc", "git", "the ci gate", "the command"]
                        .iter()
                        .any(|target| {
                            subject
                                .trim_start_matches('`')
                                .strip_prefix(target)
                                .is_some_and(|rest| !rest.starts_with(char::is_alphanumeric))
                        })
                })
            // A receipt-shaped denial is an assertion too. A real permission
            // request can ground an approval attempt without proving execution.
            || (!permission_requested
                && ["lifecycle", "run_command", "request_permissions"]
                    .iter()
                    .any(|tool| line.contains(tool))
                && (line.contains('→') || line.contains("->"))
                && line.contains("denied"))
    })
}

/// Ordinary answers use this narrow check, not the success-report heuristics:
/// an earlier failed call must not refute a subsequently repaired test run.
pub(crate) fn annotate_unobserved_probe(text: String, evidence: &Evidence) -> String {
    if !evidence.unsupported_probe_claim(&text) {
        return text;
    }
    format!(
        "{text}\n\n⚠ Harness evidence: No fresh capability checks were performed this turn. \
        No returned execution attempt supports the claimed probes; recalled history and tool \
        discovery do not establish these results. Tool availability remains unverified."
    )
}

/// Show only current advertised schemas, never resurrecting a remembered tool
/// or operation. A schema describes a route, not permission to execute it.
pub(crate) fn probe_correction(tools: &serde_json::Value) -> Option<String> {
    let schemas = tools
        .as_array()?
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            let name = function.get("name")?.as_str()?;
            matches!(name, "run_command" | "lifecycle" | "git" | "tool_search").then_some(function)
        })
        .collect::<Vec<_>>();
    if schemas.is_empty() {
        return None;
    }
    Some(format!("{} Your answer claims freshly completed capability probes, but this turn \
        has no returned execution evidence for them. Do not treat saved statements as fresh \
        observations or invent exit codes or operator decisions. Issue a real tool call for \
        the necessary check, or say explicitly that you have not checked. For build/test validation, \
        prefer lifecycle action=build only if its current schema offers it. Existing permissions \
        and approvals still apply; discovery grants no authority. Current relevant tool schemas: {}",
        super::compress::LOOP_GUIDANCE_PREFIX, serde_json::to_string(&schemas).unwrap()))
}

/// Vocabulary that asserts something was verified to work. Matched
/// case-insensitively against the whole report.
///
/// Kept SHORT and assertive on purpose. "should work", "expected to", "I
/// believe" are not on it: they claim nothing, and refuting a hedge would be
/// the spurious-refutation failure this module is biased against.
const VERIFICATION_PROSE: &[&str] = &[
    "verified",
    "working end-to-end",
    "working end to end",
    "end-to-end test",
    "tested and",
    "confirmed working",
    "fully tested",
    "all tests pass",
    "test suite passes",
    "smoke tested",
];

/// Markers that make a status-table cell a SUCCESS claim.
const SUCCESS_MARKERS: &[&str] = &["✅", "✔", "☑", "🟢"];

/// Whether the report asserts, in prose, that something was verified.
pub(crate) fn has_verification_prose(text: &str) -> bool {
    let lower = text.to_lowercase();
    VERIFICATION_PROSE.iter().any(|v| lower.contains(v))
}

/// A success marker in a Markdown table is a verification claim, but its
/// human-readable label carries no tool identity.
fn has_success_table(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with('|') && SUCCESS_MARKERS.iter().any(|marker| line.contains(marker))
    })
}

/// Append only facts this ledger establishes, preserving the exact document.
/// The report remains model-authored display, whether or not a note is added.
pub(crate) fn annotate_unsupported(text: String, evidence: &Evidence) -> String {
    if evidence.unsupported_probe_claim(&text) {
        return annotate_unobserved_probe(text, evidence);
    }
    if !has_verification_prose(&text) && !has_success_table(&text) {
        return text;
    }
    if evidence.is_silent() {
        return format!(
            "{text}\n\n⚠ capability check (#1947): this report claims verification, but \
             no tool ran in this turn. The ledger does not establish these claims."
        );
    }
    if evidence.failed > 0 {
        let total = evidence.ok + evidence.failed;
        return format!(
            "{text}\n\nℹ Tool history (#1947): {} of {total} tool call(s) returned failure \
             during this turn. This history does not establish whether those failures \
             remain unresolved or whether individual report claims are verified.",
            evidence.failed
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_negative_probe_claims_need_execution_evidence() {
        for claim in [
            "The capability probes came back. cargo --version: command not found (exit 127).",
            "I've re-probed directly this round — the toolchain is still absent.",
            "I've attempted the CI gate (cargo check) and hit a hard blocker.",
            "I attempted cargo check and it failed.",
            "• lifecycle phase=check, action=build → denied by the operator (exit 40032).",
        ] {
            let out = annotate_unsupported(claim.into(), &Evidence::default());
            assert!(out.starts_with(claim), "preserve the model's words");
            assert!(
                out.contains("No fresh capability checks were performed this turn."),
                "{out}"
            );
        }
    }

    #[test]
    fn historical_future_negated_and_quoted_probes_are_not_fresh_claims() {
        for text in [
            "Previously, the capability probes reported cargo absent.",
            "In the previous turn, I re-probed cargo directly.",
            "I have not re-probed cargo directly this round.",
            "I haven't checked cargo again.",
            "I will probe cargo now.",
            "I will do a direct re-probe of cargo next.",
            "A direct re-probe of cargo would establish availability.",
            "I have not attempted cargo check this turn.",
            "I haven't attempted cargo check this turn.",
            "I attempted cargo check, but permission was denied before execution.",
            "I will attempt cargo check after approval.",
            "I attempted to repair the cargo configuration, but have not run it.",
            "I attempted GitHub login to inspect the cargo configuration.",
            "Earlier I attempted cargo check and it failed.",
            "Previously: lifecycle phase=check, action=build → denied by the operator.",
            "> lifecycle phase=check, action=build → denied by the operator.",
            "```text\nlifecycle phase=check, action=build → denied by the operator.\n```",
            "If approved, lifecycle phase=check, action=build → cargo check.",
            "If lifecycle phase=check, action=build → denied, stop and ask the operator.",
            "- If lifecycle phase=check, action=build → denied, stop.",
            "lifecycle phase=check, action=build → might be denied by the operator.",
            "\"lifecycle phase=check, action=build → denied by the operator.\"",
            "> I've re-probed directly this round — the toolchain is still absent.",
            "You said: The capability probes came back. cargo was missing.",
            "```text\nThe capability probes came back. cargo was missing.\n```",
        ] {
            assert_eq!(
                annotate_unsupported(text.into(), &Evidence::default()),
                text
            );
        }
    }

    #[test]
    fn actual_execution_attempts_ground_probe_claims_even_when_denied() {
        let text = "I've re-probed directly this round — the toolchain is still absent.";
        for outcome in [
            crate::ExecOutcome::Passed,
            crate::ExecOutcome::Failed,
            crate::ExecOutcome::Denied,
            crate::ExecOutcome::Unavailable,
            crate::ExecOutcome::TimedOut,
        ] {
            let mut call = event("run_command", false);
            call.execution = Some(outcome);
            assert_eq!(
                annotate_unsupported(text.into(), &Evidence::from_events(&[call])),
                text
            );
        }
        let history = Evidence::from_events(&[event("resume_context", true)]);
        assert!(annotate_unsupported(text.into(), &history)
            .contains("No fresh capability checks were performed this turn."));
    }

    #[test]
    fn a_real_permission_request_grounds_denial_but_not_a_claimed_execution() {
        let evidence = Evidence::from_events(&[event("request_permissions", true)]);
        let denied = "lifecycle phase=check, action=build → denied by the operator.";
        assert_eq!(annotate_unobserved_probe(denied.into(), &evidence), denied);
        let claim = "I've attempted the CI gate (cargo check) and hit a hard blocker.";
        assert!(annotate_unobserved_probe(claim.into(), &evidence)
            .contains("No fresh capability checks were performed this turn."));
    }

    #[test]
    fn correction_preserves_current_scoped_schema_and_is_bounded() {
        let evidence = Evidence::default();
        let claim = "I've re-probed directly this round — the toolchain is still absent.";
        let tools = serde_json::json!([super::super::git_tool::definition_for_read_scope(
            &crate::Scope::none()
        )]);
        let mut used = false;
        assert!(evidence
            .correction(claim, &tools, &mut used, false)
            .is_none());
        assert!(!used, "a cancelled/exhausted turn consumes no correction");
        let nudge = evidence.correction(claim, &tools, &mut used, true).unwrap();
        assert!(nudge.contains("branch-list"));
        assert!(
            !nudge.contains("\"commit\""),
            "never restore a hidden operation"
        );
        assert!(nudge.contains("discovery grants no authority"));
        assert!(evidence
            .correction(claim, &tools, &mut used, true)
            .is_none());
        used = false;
        assert!(evidence
            .correction(claim, &serde_json::json!([]), &mut used, true)
            .is_none());
        assert!(!used, "no correction can advertise an absent tool");
    }

    fn event(tool: &str, ok: bool) -> ToolEvent {
        ToolEvent::from_call(tool, &serde_json::json!({"k": "v"}), ok, Some(1))
    }

    #[test]
    fn ordinary_report_labels_are_not_tool_identities() {
        let report = "| Gate | Result |\n|---|---|\n\
            | cargo build --workspace | ✅ green |\n\
            | cargo clippy --lib (newt-core) | ✅ no warnings |\n\
            | cargo fmt --all -- --check | ✅ passes |\n\
            | Guard tests (16) | ✅ 16 passed |\n";
        let mut evidence = Evidence::default();
        evidence.record("run_command", true, Some(crate::ExecOutcome::Passed));
        assert_eq!(
            annotate_unsupported(report.to_string(), &evidence),
            report,
            "arbitrary Markdown labels do not identify tool calls or acquire attestation"
        );
    }

    #[test]
    fn past_failures_remain_history_without_claiming_they_are_repaired_or_current() {
        for later_tool in ["run_command", "lifecycle"] {
            let report = "Current build verified; an earlier attempt failed.\n";
            let mut evidence = Evidence::default();
            evidence.record("run_command", false, Some(crate::ExecOutcome::Failed));
            evidence.record(later_tool, true, Some(crate::ExecOutcome::Passed));
            let out = annotate_unsupported(report.to_string(), &evidence);
            assert!(out.starts_with(report), "report must survive: {out}");
            assert!(out.contains("Tool history"), "{out}");
            assert!(
                out.contains("1 of 2 tool call(s) returned failure"),
                "{out}"
            );
            assert!(out.contains("does not establish"), "{out}");
            for invented_claim in [
                "re-run the checks",
                "this report claims verification, but",
                "no tool call named",
                "failure was repaired",
            ] {
                assert!(!out.contains(invented_claim), "{out}");
            }
        }
    }

    /// The #1947 report, as shipped: a ten-row all-✅ table plus the prose
    /// claim, over a turn that touched none of it.
    fn the_failing_report() -> String {
        let rows = [
            "list_audio_devices",
            "stt_transcribe",
            "tts_speak",
            "vad_detect",
            "converse",
            "daemon_start",
            "daemon_stop",
            "config_load",
            "audio_capture",
            "audio_playback",
        ]
        .iter()
        .map(|r| format!("| {r} | ✅ |"))
        .collect::<Vec<_>>()
        .join("\n");
        format!(
            "# Voice stack\n\n| Component | Status |\n|---|---|\n{rows}\n\n\
             MCP stdio transport verified working end-to-end.\n"
        )
    }

    /// Tool-like labels in arbitrary report content are still not typed
    /// identities. Even a named MCP call cannot attest a whole capability.
    #[test]
    fn report_labels_do_not_acquire_identity_from_resembling_mcp_tools() {
        let evidence = Evidence::from_events(&[event("list_audio_devices", true)]);
        assert_eq!(
            annotate_unsupported(the_failing_report(), &evidence),
            the_failing_report(),
            "rendering does not turn an unverified report into an attestation"
        );
    }

    /// Successful calls do not require a history note or certify the report.
    #[test]
    fn a_report_over_successful_calls_is_returned_unchanged() {
        let report = "# Done\n\n| Component | Status |\n|---|---|\n\
                      | stt_transcribe | ✅ |\n| converse | ✅ |\n\n\
                      Verified working end-to-end.\n"
            .to_string();
        let evidence = Evidence::from_events(&[
            event("voice__stt_transcribe", true),
            event("voice__converse", true),
        ]);
        assert_eq!(
            annotate_unsupported(report.clone(), &evidence),
            report,
            "successful calls do not require a historical failure note"
        );
    }

    /// **Anti-vacuous twin 2: the SAME report against an empty ledger is
    /// refuted.** The pair is the proof — same input text, opposite verdict,
    /// and the only thing that changed is the evidence.
    #[test]
    fn the_same_report_against_an_empty_ledger_is_refuted() {
        let report = "# Done\n\n| Component | Status |\n|---|---|\n\
                      | stt_transcribe | ✅ |\n| converse | ✅ |\n\n\
                      Verified working end-to-end.\n"
            .to_string();
        let corroborated = Evidence::from_events(&[
            event("voice__stt_transcribe", true),
            event("voice__converse", true),
        ]);
        let silent = Evidence::default();

        assert_eq!(annotate_unsupported(report.clone(), &corroborated), report);
        let refuted = annotate_unsupported(report.clone(), &silent);
        assert_ne!(refuted, report, "an empty ledger must refute");
        assert!(refuted.contains("no tool ran in this turn"), "{refuted}");
    }

    /// Failures are observable history, not proof about a report's subject.
    #[test]
    fn failed_calls_are_reported_as_history_without_inferred_subjects() {
        let evidence = Evidence::from_events(&[
            event("run_command", false),
            event("run_command", false),
            event("read_file", true),
        ]);
        let out = annotate_unsupported("All green. Confirmed working.\n".to_string(), &evidence);
        assert!(
            out.contains("2 of 3 tool call(s) returned failure"),
            "{out}"
        );
    }

    /// **A report that claims nothing is never annotated.** The check reads
    /// every report; it must be silent on the ones making no assertion, or
    /// it is a tax on honest output rather than a check.
    #[test]
    fn a_report_making_no_claim_is_never_annotated() {
        for text in [
            "# Status\n\nI looked at the parser and it is complex.\n",
            "# Plan\n\n| Step | Owner |\n|---|---|\n| refactor | me |\n",
            // A hedge is not a claim. Refuting one would be the
            // spurious-refutation failure this module is biased against.
            "# Notes\n\nThis should work once the daemon starts.\n",
        ] {
            let evidence = Evidence::default();
            assert_eq!(
                annotate_unsupported(text.to_string(), &evidence),
                text,
                "a report claiming nothing must pass untouched: {text}"
            );
        }
    }

    #[test]
    fn only_success_marked_tables_trigger_history_notes() {
        assert!(has_success_table(
            "| Gate | Result |\n|---|---|\n| build | ✅ |"
        ));
        for text in [
            "| Gate | Result |\n|---|---|",
            "| build | ❌ |\n| tests | pending |",
            "A quoted ✅ glyph is not a status table.",
        ] {
            assert!(!has_success_table(text), "{text}");
        }
    }
}
