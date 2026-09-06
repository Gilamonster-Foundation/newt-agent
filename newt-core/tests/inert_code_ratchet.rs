//! Ratchet for the detector-generated inert-code inventory.
//!
//! Inert code is code that exists and creates a false affordance: an empty
//! override masks a meaningful default, a public island has no production
//! consumer, a parsed knob never changes a decision, or a computation is
//! discarded or measures the wrong thing. These species are heterogeneous;
//! pretending one textual predicate proves semantic liveness would create the
//! noisy gate this ratchet is meant to avoid.
//!
//! Phase 1 generated and manually validated the inventory with AST/token,
//! data-flow, script-parser, and documentation-reference detectors. This file
//! freezes that result in two layers. The registered witness layer names every
//! source needle and requires a reason in the same file. Removing a site or
//! its reason therefore cannot look like a clean tree: the count falls loudly
//! and asks the paying change to lower the ratchet. Marker identity prevents
//! an acknowledged unit from hiding behind an unchanged aggregate.
//!
//! **What that does NOT cover, stated so nobody has to discover it.** The
//! check is textual. A site repaired *semantically* — a caller added to
//! something registered as having none, a field that starts being read —
//! leaves its declaration and marker in place, so every count, identity and
//! test stays green while the row's present-tense justification has quietly
//! become false. Deletion is caught; wiring is not. Treat this layer as
//! "these registered witnesses still read as inert", never as a live liveness
//! analysis.
//!
//! Workflow syntax is also detected independently: every JavaScript-family
//! file under the NON-HIDDEN paths of `scripts/` is parsed in its real
//! dialect and the exact failure set is ratcheted. The qualifier is load
//! bearing — hidden directories are skipped by the walker, so a file under
//! something like `scripts/.fixtures/` is outside this detector's domain. That detector can discover a new unregistered failure.
//! The remaining wrong-number, wildcard, and consumer-liveness findings stay
//! registered witnesses because pretending to infer arbitrary intent with a
//! textual heuristic would create the noisy detector this gate rejects.
//!
//! Deliberately inert implementations are a separate, exact set of REGISTERED
//! sanctions. "Registered" is the honest word: the suite proves every listed
//! sanction is resolved as one, not that the list is every deliberate no-op in
//! the tree. A new unmarked no-op is not discovered by anything here.
//! Serde/wire DTOs, PyO3 registrations, `thiserror` conversions, and Clap
//! variants are not sanctions: generated or external construction is a real
//! consumer, so those families are excluded structurally before inventorying.

mod common;

use common::{for_each_production_line, production_roots, workspace_root};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const KNOWN_INERT_UNITS: usize = 51;
const KNOWN_SANCTIONS: usize = 16;
const KNOWN_SCRIPT_PARSE_FAILURES: usize = 1;
const WORKFLOW_COMPILER: &str = r#"
const fs = require('node:fs');
const path = process.argv[1];
const source = fs.readFileSync(path, 'utf8');
const body = source.replace(/^(\s*)export\s+const\s+meta\s*=/m, '$1const meta =');
if (body === source) {
  throw new SyntaxError('workflow must declare `export const meta =`');
}
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
new AsyncFunction('args', body);
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Resolution {
    Wire,
    Delete,
    Document,
    Sanction,
}

impl Resolution {
    const fn label(self) -> &'static str {
        match self {
            Self::Wire => "WIRE",
            Self::Delete => "DELETE",
            Self::Document => "DOCUMENT",
            Self::Sanction => "SANCTION",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Site {
    file: &'static str,
    needle: &'static str,
    occurrences: usize,
    search_raw: bool,
    reason: ReasonRequirement,
    scope: Option<&'static str>,
}

#[derive(Clone, Copy, Debug)]
enum ReasonRequirement {
    Marker,
    Existing(&'static str),
}

const fn site(file: &'static str, needle: &'static str) -> Site {
    Site {
        file,
        needle,
        occurrences: 1,
        search_raw: false,
        reason: ReasonRequirement::Marker,
        scope: None,
    }
}

const fn sites(file: &'static str, needle: &'static str, occurrences: usize) -> Site {
    Site {
        file,
        needle,
        occurrences,
        search_raw: false,
        reason: ReasonRequirement::Marker,
        scope: None,
    }
}

const fn raw_site(file: &'static str, needle: &'static str) -> Site {
    Site {
        file,
        needle,
        occurrences: 1,
        search_raw: true,
        reason: ReasonRequirement::Marker,
        scope: None,
    }
}

const fn explained(file: &'static str, needle: &'static str, local_reason: &'static str) -> Site {
    Site {
        file,
        needle,
        occurrences: 1,
        search_raw: false,
        reason: ReasonRequirement::Existing(local_reason),
        scope: None,
    }
}

const fn scoped_explained(
    file: &'static str,
    scope: &'static str,
    needle: &'static str,
    local_reason: &'static str,
) -> Site {
    Site {
        file,
        needle,
        occurrences: 1,
        search_raw: false,
        reason: ReasonRequirement::Existing(local_reason),
        scope: Some(scope),
    }
}

const fn scoped_site(file: &'static str, scope: &'static str, needle: &'static str) -> Site {
    Site {
        file,
        needle,
        occurrences: 1,
        search_raw: false,
        reason: ReasonRequirement::Marker,
        scope: Some(scope),
    }
}

#[derive(Clone, Copy, Debug)]
struct Unit {
    id: &'static str,
    resolution: Resolution,
    justification: &'static str,
    sites: &'static [Site],
}

macro_rules! unit {
    ($id:literal, $resolution:ident, $reason:literal, [$($site:expr),+ $(,)?]) => {
        Unit {
            id: $id,
            resolution: Resolution::$resolution,
            justification: $reason,
            sites: &[$($site),+],
        }
    };
}

const DEBT: &[Unit] = &[
    unit!(
        "X02",
        Wire,
        "documented SSH authority types and gate have no production caller.",
        [
            site("newt-core/src/ssh_caveats.rs", "pub struct SshCaveats"),
            site("newt-core/src/ssh_caveats.rs", "pub enum GitNetVerb"),
            site(
                "newt-core/src/ssh_caveats.rs",
                "pub fn git_over_ssh_permitted"
            ),
        ]
    ),
    unit!(
        "X03",
        Delete,
        "multi-attach session island has no production consumer; SessionId stays live.",
        [
            site("newt-core/src/session.rs", "pub enum OutputStream"),
            site("newt-core/src/session.rs", "pub struct OutputChunk"),
            site("newt-core/src/session.rs", "pub enum AttachRole"),
            site("newt-core/src/session.rs", "pub struct AttachId"),
            site("newt-core/src/session.rs", "pub trait OutputSink"),
            site("newt-core/src/session.rs", "pub enum InputRefused"),
            site("newt-core/src/session.rs", "pub struct AcceptedTurn"),
            site("newt-core/src/session.rs", "pub struct SessionState"),
            site("newt-core/src/session.rs", "pub struct SessionRegistry"),
        ]
    ),
    unit!(
        "X04",
        Wire,
        "dispatch strategy has no production consumer and FanOut measures candidates, not slots.",
        [
            site("newt-scheduler/src/lib.rs", "pub slots: usize"),
            site("newt-scheduler/src/lib.rs", "pub enum DispatchStrategy"),
            site("newt-scheduler/src/lib.rs", "pub fn strategy(&self"),
            site(
                "newt-scheduler/src/lib.rs",
                "n => DispatchStrategy::FanOut(n)"
            ),
        ]
    ),
    unit!(
        "X05",
        Delete,
        "registry taxonomy variants have no production construction or match.",
        [
            site("newt-core/src/kit.rs", "    Mode,"),
            site("newt-core/src/kit.rs", "    RequestKnobs,"),
            site("newt-core/src/kit.rs", "    Reasoning,"),
            site("newt-core/src/kit.rs", "    Structure,"),
            site("newt-core/src/kit.rs", "    TuiOnly,"),
        ]
    ),
    unit!(
        "X06",
        Delete,
        "four exported staging helpers have zero consumers.",
        [
            site("newt-core/src/atomic_fs.rs", "pub fn stable_path"),
            site("newt-core/src/atomic_fs.rs", "pub fn stage_file("),
            site("newt-core/src/atomic_fs.rs", "pub fn stage_private_file"),
            site(
                "newt-core/src/atomic_fs.rs",
                "pub fn stage_file_with_permissions"
            ),
        ]
    ),
    unit!(
        "X07",
        Delete,
        "obsolete code-file projection helpers have zero consumers.",
        [
            site(
                "newt-core/src/api_surface.rs",
                "pub fn code_file_extensions"
            ),
            site("newt-core/src/api_surface.rs", "pub fn path_is_code_file"),
        ]
    ),
    unit!(
        "X08",
        Wire,
        "project-pack loading and merge behavior is tested but never entered in production.",
        [
            site(
                "newt-core/src/project_model.rs",
                "pub fn merge_project_packs"
            ),
            site(
                "newt-core/src/project_model.rs",
                "pub fn load_project_packs_from_dir"
            ),
        ]
    ),
    unit!(
        "X09",
        Delete,
        "legacy operator-email wrapper has no caller; matched-pair identity resolution is live.",
        [site(
            "newt-core/src/agent_identity.rs",
            "pub fn default_operator_email"
        ),]
    ),
    unit!(
        "X10",
        Delete,
        "public Anthropic wrappers have no entry caller; the private implementation is live.",
        [
            site(
                "newt-core/src/agentic/mod.rs",
                "pub async fn anthropic_chat_complete("
            ),
            site(
                "newt-core/src/agentic/mod.rs",
                "pub async fn anthropic_chat_complete_with_prompt("
            ),
        ]
    ),
    unit!(
        "X11",
        Delete,
        "code-evidence wrapper is only reexported; ranked retrieval is the live path.",
        [site(
            "newt-core/src/agentic/semantic.rs",
            "pub fn code_evidence_block"
        ),]
    ),
    unit!(
        "X12",
        Delete,
        "offload-and-prompt wrapper is only reexported; collaborator dispatch is live.",
        [site(
            "newt-core/src/agentic/tools.rs",
            "pub async fn execute_tool_with_offload_and_prompt("
        ),]
    ),
    unit!(
        "X13",
        Delete,
        "transcript role constant is reexported but no renderer reads it.",
        [site(
            "newt-core/src/agentic/driver.rs",
            "pub const VISIBLE_TRANSCRIPT_ROLES"
        ),]
    ),
    unit!(
        "X15",
        Delete,
        "feature-gated schema helpers have zero consumers and the scalar macro supersedes them.",
        [
            site("newt-interaction/src/tag.rs", "pub fn name_schema"),
            site("newt-interaction/src/tag.rs", "pub fn non_empty_schema"),
        ]
    ),
    unit!(
        "X16",
        Delete,
        "publishable-revision helper has zero consumers and only exposes an existing field.",
        [site(
            "newt-interaction/src/lifecycle.rs",
            "pub fn publishable_revision"
        ),]
    ),
    unit!(
        "X17",
        Delete,
        "idempotency key display helper has zero consumers and adds no semantics.",
        [site(
            "newt-interaction/src/resolution.rs",
            "pub fn key_display"
        ),]
    ),
    unit!(
        "X18",
        Delete,
        "setup wrapper has zero workspace callers; the with-model entry point is live.",
        [
            site("newt-tui/src/lib.rs", "pub async fn run_setup_target("),
            site(
                "newt-tui/src/lib.rs",
                "run_setup_target_with_model(target, token_env, token_file, None, yes, config_path)"
            ),
        ]
    ),
    unit!(
        "X19",
        Delete,
        "grounding API is a closed tested island with no production entry point.",
        [
            site("newt-core/src/grounding.rs", "pub const DEFAULT_NGRAM"),
            site("newt-core/src/grounding.rs", "pub const MIN_QUOTE_TOKENS"),
            site("newt-core/src/grounding.rs", "pub struct GroundingConfig"),
            site("newt-core/src/grounding.rs", "pub struct Span"),
            site("newt-core/src/grounding.rs", "pub enum Ungrounded"),
            site("newt-core/src/grounding.rs", "pub struct Grounding {"),
            site("newt-core/src/grounding.rs", "pub fn check("),
        ]
    ),
    unit!(
        "X20",
        Delete,
        "transcript rendering island has only test, documentation, and reexport consumers.",
        [
            site(
                "newt-core/src/agentic/transcript.rs",
                "pub enum TranscriptRole"
            ),
            site(
                "newt-core/src/agentic/transcript.rs",
                "pub struct TranscriptLine"
            ),
            site(
                "newt-core/src/agentic/transcript.rs",
                "pub struct TranscriptStyle"
            ),
            site(
                "newt-core/src/agentic/transcript.rs",
                "pub fn transcript_lines("
            ),
            site(
                "newt-core/src/agentic/transcript.rs",
                "pub fn transcript_lines_styled("
            ),
        ]
    ),
    unit!(
        "X21",
        Wire,
        "positive retry is not in the live zero-retry path and most outcome fields are unread.",
        [
            explained(
                "newt-core/src/verify_gate.rs",
                "pub trait RetryRerun",
                "the live loop's increment-2a path; bumping to a real"
            ),
            explained(
                "newt-core/src/verify_gate.rs",
                "pub struct RetryOutcome",
                "the live loop's increment-2a path; bumping to a real"
            ),
            explained(
                "newt-core/src/verify_gate.rs",
                "pub accepted: bool",
                "the live loop's increment-2a path; bumping to a real"
            ),
            explained(
                "newt-core/src/verify_gate.rs",
                "pub retries_used: u32",
                "the live loop's increment-2a path; bumping to a real"
            ),
            explained(
                "newt-core/src/verify_gate.rs",
                "pub outstanding_modules: Vec<String>",
                "the live loop's increment-2a path; bumping to a real"
            ),
            explained(
                "newt-core/src/verify_gate.rs",
                "pub async fn apply_revert_retry",
                "the live loop's increment-2a path; bumping to a real"
            ),
        ]
    ),
    unit!(
        "X22",
        Delete,
        "panel and voting API is a closed tested island with no production caller.",
        [
            site("newt-scheduler/src/panel.rs", "pub struct VoiceSpec"),
            site("newt-scheduler/src/panel.rs", "pub struct PanelConfig"),
            site("newt-scheduler/src/panel.rs", "pub struct Vote"),
            site("newt-scheduler/src/panel.rs", "pub enum PanelStatus"),
            site("newt-scheduler/src/panel.rs", "pub struct PanelOutcome"),
            site("newt-scheduler/src/panel.rs", "pub trait Verify"),
            site("newt-scheduler/src/panel.rs", "pub async fn run_panel"),
        ]
    ),
    unit!(
        "X23",
        Delete,
        "nudger schema and registry is explicitly dormant and has no external consumer.",
        [
            explained(
                "newt-core/src/nudger.rs",
                "pub enum KnobScope",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub struct KnownKnob",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub const KNOWN_KNOBS",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub fn known_knob",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub struct NudgerProfile",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub fn parse_profile",
                "for now — entirely dormant: nothing consumes it yet"
            ),
            explained(
                "newt-core/src/nudger.rs",
                "pub struct ValidationReport",
                "for now — entirely dormant: nothing consumes it yet"
            ),
        ]
    ),
    unit!(
        "X24",
        Wire,
        "SAS confirmation is tested security plumbing with no production enrollment caller.",
        [
            site("newt-core/src/sas_confirm.rs", "pub enum SasVerdict"),
            site("newt-core/src/sas_confirm.rs", "pub fn recompute_sas"),
            site("newt-core/src/sas_confirm.rs", "pub fn confirm_question"),
            site("newt-core/src/sas_confirm.rs", "pub fn confirm_enrollment"),
        ]
    ),
    unit!(
        "X25",
        Wire,
        "secret passphrase preflight and retry helpers are tested but absent from the TUI path.",
        [
            site("newt-core/src/secrets.rs", "pub fn needs_passphrase"),
            site("newt-core/src/secrets.rs", "pub fn try_unlock"),
        ]
    ),
    unit!(
        "X26",
        Delete,
        "markdown table tidy function has only test and documentation consumers.",
        [
            site(
                "newt-core/src/agentic/mod.rs",
                "pub fn tidy_markdown_tables(src: &str) -> String {\n    markdown_table_formatter::format_tables(src)\n}"
            ),
            site(
                "newt-core/src/agentic/mod.rs",
                "pub fn tidy_markdown_tables(src: &str) -> String {\n    src.to_string()\n}"
            ),
        ]
    ),
    unit!(
        "X27",
        Wire,
        "terminal-echo authority helper is claimed as live but has no production caller.",
        [site(
            "newt-core/src/permission_challenge.rs",
            "pub fn requires_terminal_echo"
        ),]
    ),
    unit!(
        "X28",
        Wire,
        "memory prefetch default and manager fanout have no production caller.",
        [
            site("newt-core/src/memory.rs", "async fn prefetch(&self, _query"),
            site("newt-core/src/memory.rs", "pub async fn prefetch_all"),
        ]
    ),
    unit!(
        "X29",
        Wire,
        "pre-compression hook has a real override but its manager entry point is never called.",
        [
            scoped_site(
                "newt-core/src/memory.rs",
                "pub trait MemoryProvider: Send + Sync",
                "async fn on_pre_compress(&self, _messages"
            ),
            scoped_site(
                "newt-core/src/memory.rs",
                "impl MemoryProvider for Summarizing",
                "async fn on_pre_compress(&self, _messages"
            ),
            site(
                "newt-core/src/memory.rs",
                "pub async fn on_pre_compress(&self, messages"
            ),
        ]
    ),
    unit!(
        "X30",
        Wire,
        "session-end memory hook and manager fanout have no production caller.",
        [
            site(
                "newt-core/src/memory.rs",
                "async fn on_session_end(&mut self, _messages"
            ),
            site(
                "newt-core/src/memory.rs",
                "pub async fn on_session_end(&mut self, messages"
            ),
        ]
    ),
    unit!(
        "X31",
        Delete,
        "obsolete sync_all wrapper has only tests; active code uses the task-aware variant.",
        [site(
            "newt-core/src/memory.rs",
            "pub async fn sync_all(&mut self"
        ),]
    ),
    unit!(
        "F02",
        Delete,
        "headless turn outcome stores a streaming flag no consumer reads.",
        [site(
            "newt-core/src/agentic/driver.rs",
            "pub was_streamed: bool"
        ),]
    ),
    unit!(
        "F03",
        Delete,
        "stored pre-bridge estimate is unread after framing overhead is computed.",
        [site(
            "newt-core/src/agentic/responses_compaction.rs",
            "pub(super) pre_bridge_estimate: usize"
        ),]
    ),
    unit!(
        "F04",
        Wire,
        "max_age_days is parsed and defaulted but no retention decision reads it.",
        [site("newt-core/src/config.rs", "pub max_age_days: u64"),]
    ),
    unit!(
        "F05",
        Wire,
        "chat_style is parsed and defaulted but never changes TUI rendering.",
        [site("newt-core/src/config.rs", "pub chat_style: ChatStyle"),]
    ),
    unit!(
        "F06",
        Wire,
        "shell admission and mutation knobs are parsed but neither shell path consults them.",
        [
            site("newt-core/src/config.rs", "pub allow_shell_commands: bool"),
            site("newt-core/src/config.rs", "pub allow_shell_mutations: bool"),
        ]
    ),
    unit!(
        "F07",
        Wire,
        "bundle about text is parsed but never reaches the promised startup banner.",
        [site("newt-core/src/config.rs", "pub about: Option<String>"),]
    ),
    unit!(
        "F08",
        Delete,
        "user-settable dynamic-catalog knob is explicitly reserved and unused.",
        [explained(
            "newt-core/src/config/tool_exposure.rs",
            "pub supports_dynamic_catalog: bool",
            "Reserved for the per-round working-set pass; unused in Pass 1"
        ),]
    ),
    unit!(
        "F09",
        Wire,
        "serialized plan aggregation is never read or matched by the executor.",
        [
            site("newt-core/src/plan.rs", "pub aggregation: Aggregation"),
            site("newt-core/src/plan.rs", "pub enum Aggregation"),
            site("newt-core/src/plan.rs", "LastWins,"),
            site("newt-core/src/plan.rs", "Reduce,"),
            site("newt-core/src/plan.rs", "Custom,"),
        ]
    ),
    unit!(
        "F10",
        Wire,
        "parallel_ok is serialized but ready siblings still run sequentially.",
        [site("newt-core/src/plan.rs", "pub parallel_ok: bool"),]
    ),
    unit!(
        "F11",
        Wire,
        "adoption computes warm-model and pin-conflict facts that the UI never reads.",
        [
            site("newt-core/src/backend_probe.rs", "pub adopted_warm: bool"),
            site(
                "newt-core/src/backend_probe.rs",
                "pub pin_conflict: Option<String>"
            ),
        ]
    ),
    unit!(
        "F12",
        Wire,
        "exposure planning computes hidden and token-budget telemetry that production drops.",
        [
            site(
                "newt-core/src/agentic/tools/exposure.rs",
                "pub hidden: Vec<String>"
            ),
            site(
                "newt-core/src/agentic/tools/exposure.rs",
                "pub exposed_tokens: usize"
            ),
            site(
                "newt-core/src/agentic/tools/exposure.rs",
                "pub budget_tokens: Option<usize>"
            ),
        ]
    ),
    unit!(
        "F14",
        Wire,
        "role tier is displayed but never participates in backend placement.",
        [site(
            "newt-core/src/role_profile.rs",
            "pub tier: Option<Tier>"
        ),]
    ),
    unit!(
        "F15",
        Wire,
        "empty backend tiers are the default and match every requested tier.",
        [
            site(
                "newt-scheduler/src/lib.rs",
                "self.tiers.is_empty() || self.tiers.contains(&tier)"
            ),
            site(
                "newt-core/src/config.rs",
                "pub model_path: Option<String>,\n    #[serde(default)]\n    pub tiers: Vec<Tier>,"
            ),
        ]
    ),
    unit!(
        "F16",
        Wire,
        "ACP worker loads pricing then discards it while usage and cost stay absent.",
        [
            explained(
                "newt-acp-worker/src/server.rs",
                "let _ = pricing",
                "suppress unused warning until token usage is wired"
            ),
            explained(
                "newt-acp-worker/src/server.rs",
                "usage: None",
                "suppress unused warning until token usage is wired"
            ),
            explained(
                "newt-acp-worker/src/server.rs",
                "cost_usd: None",
                "suppress unused warning until token usage is wired"
            ),
        ]
    ),
    unit!(
        "F17",
        Delete,
        "tab retry computes a degraded boolean and immediately discards it.",
        [
            site(
                "newt-tui/src/tab_switch.rs",
                "let still_degraded = restored.degraded.is_some()"
            ),
            site("newt-tui/src/tab_switch.rs", "let _ = still_degraded"),
        ]
    ),
    unit!(
        "F18",
        Wire,
        "permission refusal detail is promised to audit but explicitly discarded.",
        [
            site(
                "newt-tui/src/permissions.rs",
                "refusal: &newt_interaction::Refusal"
            ),
            site("newt-tui/src/permissions.rs", "let _ = refusal"),
        ]
    ),
    unit!(
        "F19",
        Delete,
        "flowchart renderer imports NODE_H only to discard it.",
        [
            site(
                "newt-core/src/markup/extension/flowchart/svg.rs",
                "use super::layout::{Layout, NODE_H};"
            ),
            site(
                "newt-core/src/markup/extension/flowchart/svg.rs",
                "let _ = NODE_H;"
            ),
        ]
    ),
    unit!(
        "S01",
        Wire,
        "workflow declares the REPO lexical binding twice and cannot load.",
        [sites(
            "scripts/eval/workflows/propose-verify.workflow.js",
            "const REPO = argv.repo",
            2
        ),]
    ),
    unit!(
        "S02",
        Wire,
        "documentation checker scans root and docs only, omitting tracked script reports.",
        [
            site("scripts/docs_check.py", "REPO.glob(\"*.md\")"),
            site("scripts/docs_check.py", "(REPO / \"docs\").rglob(\"*.md\")"),
        ]
    ),
    unit!(
        "S03",
        Wire,
        "two evaluation reports link to a documentation path that cannot resolve.",
        [
            sites(
                "scripts/eval/results/ratchet-2026-06-27.md",
                "../../docs/design/the-ceiling-is-the-harness.md",
                2
            ),
            sites(
                "scripts/eval/results/real-548-2026-06-27.md",
                "../../docs/design/the-ceiling-is-the-harness.md",
                2
            ),
        ]
    ),
    unit!(
        "S04",
        Document,
        "three documents describe nonexistent forge resolver code as shipped or current.",
        [
            site(
                "docs/decisions/structural_parsing_over_regex.md",
                "newt-tui/src/forge_context.rs"
            ),
            site(
                "docs/decisions/structural_parsing_over_regex.md",
                "newt-core/src/forge_resolvers.rs"
            ),
            site(
                "docs/design/transparent_command_layer.md",
                "the first shipped\nslice — `newt-tui/src/forge_context.rs`"
            ),
            site(
                "docs/design/transparent_command_layer.md",
                "newt-core/src/forge_resolvers.rs"
            ),
            site(
                "docs/design/command_plugin_runtime.md",
                "newt-tui::forge_context"
            ),
            site(
                "docs/design/command_plugin_runtime.md",
                "newt-core::forge_resolvers"
            ),
            site(
                "docs/design/command_plugin_runtime.md",
                "resolve_forge_urls"
            ),
        ]
    ),
    unit!(
        "S05",
        Document,
        "crew runner names a remote MeshCrewRunner type that does not exist.",
        [raw_site(
            "newt-cli/src/crew_runner.rs",
            "`MeshCrewRunner` (wyvern-agent#42) is the remote sibling"
        ),]
    ),
];

const SANCTIONS: &[Unit] = &[
    unit!(
        "A01",
        Sanction,
        "fixed-verification workspaces intentionally ignore per-subtask command changes.",
        [explained(
            "newt-scheduler/src/crew.rs",
            "fn set_test_command(&mut self, _cmd: &str) {}",
            "Default no-op, so a fixed-verification workspace (most mocks) is unaffected"
        ),]
    ),
    unit!(
        "A02",
        Sanction,
        "non-Windows has no platform process environment to preserve.",
        [explained(
            "plugins-protocol/src/client.rs",
            "fn preserve_platform_process_env(_cmd: &mut Command) {}",
            "Non-Windows processes have no Windows platform environment to preserve"
        ),]
    ),
    unit!(
        "A03",
        Sanction,
        "release builds deliberately compile out a debug-only import failpoint.",
        [explained(
            "newt-cli/src/mcp_cmd.rs",
            "fn import_process_failpoint(_step: &str) {}",
            "Release builds deliberately compile out the debug-only import failpoint"
        ),]
    ),
    unit!(
        "A04",
        Sanction,
        "NullSink is the explicit headless sink that discards every progress event.",
        [
            scoped_explained(
                "newt-core/src/progress/mod.rs",
                "impl ProgressSink for NullSink",
                "fn frame(&mut self, _task: TaskId, _at_ms: u64, _frame: &Frame) {}",
                "A sink that discards everything"
            ),
            scoped_explained(
                "newt-core/src/progress/mod.rs",
                "impl ProgressSink for NullSink",
                "fn record(&mut self, _task: TaskId, _at_ms: u64, _event: &Durable) {}",
                "A sink that discards everything"
            ),
        ]
    ),
    unit!(
        "A05",
        Sanction,
        "LatestFrame holds view state only, so durable records are outside its contract.",
        [scoped_explained(
            "newt-core/src/progress/mod.rs",
            "impl ProgressSink for LatestFrame",
            "fn record(&mut self, _task: TaskId, _at_ms: u64, _event: &Durable) {}",
            "Durable events are not this sink's business — it holds view state only"
        ),]
    ),
    unit!(
        "A06",
        Sanction,
        "Scrollback records durable events and deliberately drops transient frames.",
        [scoped_explained(
            "newt-core/src/progress/mod.rs",
            "impl ProgressSink for Scrollback",
            "fn frame(&mut self, _task: TaskId, _at_ms: u64, _frame: &Frame) {}",
            "its [`frame`](ProgressSink::frame) is a genuine no-op — not \"a no-op today\""
        ),]
    ),
    unit!(
        "A07",
        Sanction,
        "default completed-spill discard has no bookkeeping to clear.",
        [explained(
            "newt-core/src/agentic/mod.rs",
            "fn discard(&self) {}",
            "No-op when nothing is up"
        ),]
    ),
    unit!(
        "A08",
        Sanction,
        "completed-spill archive commits an excerpt but owns no terminal frame.",
        [
            explained(
                "newt-tui/src/completed_spill.rs",
                "fn render_completed(&self, _output: &str, _width: usize, _max_height: usize) -> usize",
                "This archive paints nothing"
            ),
            explained(
                "newt-tui/src/completed_spill.rs",
                "fn erase(&self) {}",
                "Nothing to rewind"
            ),
        ]
    ),
    unit!(
        "A09",
        Sanction,
        "ephemeral row restore defers to the shared ticker to avoid a prompt race.",
        [explained(
            "newt-core/src/tty/row.rs",
            "fn restore(&self) {",
            "the shared ticker repaints within one frame"
        ),]
    ),
    unit!(
        "A10",
        Sanction,
        "Spinner::finish consumes the handle so Drop performs the idempotent teardown.",
        [explained(
            "newt-core/src/tty/spinner.rs",
            "pub fn finish(self) {",
            "`Drop` runs `finish`"
        ),]
    ),
    unit!(
        "A11",
        Sanction,
        "cockpit registers only for suspension and does not paint through the arbiter.",
        [
            explained(
                "newt-tui/src/cockpit/presenter.rs",
                "fn erase(&self) {}",
                "The cockpit does not paint through the arbiter"
            ),
            explained(
                "newt-tui/src/cockpit/presenter.rs",
                "fn restore(&self) {}",
                "The cockpit does not paint through the arbiter"
            ),
        ]
    ),
    unit!(
        "A12",
        Sanction,
        "system-prompt and index-only memory providers own no per-turn history.",
        [
            explained(
                "newt-core/src/agents.rs",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "System-prompt-only provider; history is managed elsewhere"
            ),
            explained(
                "newt-core/src/api_surface.rs",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "System-prompt-only provider; history is managed elsewhere"
            ),
            explained(
                "newt-core/src/ffi_surface.rs",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "System-prompt-only provider; history is managed elsewhere"
            ),
            scoped_explained(
                "newt-core/src/memory.rs",
                "impl MemoryProvider for MemoryIndex",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "System-prompt-only (like NoteStore / SoulProvider)"
            ),
            scoped_explained(
                "newt-core/src/memory.rs",
                "impl MemoryProvider for SoulProvider",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "Soul is system-prompt-only"
            ),
            explained(
                "newt-core/src/notes.rs",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "NoteStore is a system-prompt-only provider"
            ),
            explained(
                "newt-core/src/project_map.rs",
                "async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}",
                "no conversation messages and no per-turn state"
            ),
        ]
    ),
    unit!(
        "A13",
        Sanction,
        "wire variant is explicitly reserved and not emitted in version one.",
        [explained(
            "newt-core/src/conversation.rs",
            "NarratedNoCall",
            "reserved; NOT emitted in v1"
        ),]
    ),
    unit!(
        "A14",
        Sanction,
        "backend provenance is serialized human audit evidence, not runtime policy.",
        [explained(
            "newt-core/src/config.rs",
            "pub provenance: Option<BackendProvenance>",
            "Written by `newt setup`; never read at runtime"
        ),]
    ),
    unit!(
        "A15",
        Sanction,
        "OCAP stubs are unreachable until their named safety gates verify.",
        [
            explained(
                "newt-core/src/ocap.rs",
                "pub fn seed_live_credential",
                "Fail-closed: refuses unless both verify"
            ),
            explained(
                "newt-core/src/ocap.rs",
                "pub fn admit_untrusted_remote",
                "Disabled while `b1-os-isolation` is open.** Fail-closed"
            ),
        ]
    ),
    unit!(
        "A16",
        Sanction,
        "phantom-reach ok parameter preserves recording-site signature symmetry.",
        [explained(
            "newt-core/src/agentic/tools/catalog.rs",
            "let _ = ok",
            "`ok` is part of the signature for symmetry with the recording site"
        ),]
    ),
];

#[derive(Debug, Default)]
struct InputStats {
    rust_files: usize,
    script_files: usize,
    markdown_files: usize,
    bytes: usize,
}

#[derive(Debug)]
struct Finding {
    id: &'static str,
    resolution: Resolution,
    files: BTreeSet<&'static str>,
}

#[derive(Debug)]
struct Incomplete {
    id: &'static str,
    failures: Vec<String>,
}

#[derive(Debug)]
struct ScanReport {
    stats: InputStats,
    findings: Vec<Finding>,
    incomplete: Vec<Incomplete>,
    markers: BTreeMap<(String, String), usize>,
}

#[derive(Debug)]
struct ScriptParseReport {
    files_read: usize,
    bytes_read: usize,
    failures: BTreeMap<String, String>,
}

#[derive(Debug, Eq, PartialEq)]
enum ScanError {
    NoInputs,
    Io(String),
}

fn normalize_whitespace(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("//!")
                .trim_start_matches("///")
                .trim_start_matches("//")
                .trim_start_matches("<!--")
                .trim_end_matches("-->")
                .trim()
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn collect_matching_files(
    root: &Path,
    dir: &Path,
    include_hidden: bool,
    accept: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) -> Result<(), ScanError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ScanError::Io(format!("reading {}: {e}", rel(root, dir))))?;
    for entry in entries {
        let entry = entry.map_err(|e| ScanError::Io(format!("reading directory entry: {e}")))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| ScanError::Io(format!("reading {} type: {e}", rel(root, &path))))?;
        if kind.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git"
                || name == ".worktrees"
                || name == "target"
                || (!include_hidden && name.starts_with('.'))
            {
                continue;
            }
            collect_matching_files(root, &path, include_hidden, accept, out)?;
        } else if kind.is_file() && accept(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn read_input(
    root: &Path,
    path: &Path,
    inputs: &mut BTreeMap<String, String>,
    stats: &mut InputStats,
) -> Result<(), ScanError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ScanError::Io(format!("reading {}: {e}", rel(root, path))))?;
    stats.bytes += text.len();
    inputs.insert(rel(root, path), text);
    Ok(())
}

fn script_parse_failures_under(root: &Path) -> Result<ScriptParseReport, ScanError> {
    let mut paths = Vec::new();
    collect_matching_files(
        root,
        &root.join("scripts"),
        false,
        &|path| {
            matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("js" | "mjs" | "cjs")
            )
        },
        &mut paths,
    )?;
    paths.sort();
    if paths.is_empty() {
        return Err(ScanError::NoInputs);
    }

    let mut failures = BTreeMap::new();
    let mut bytes_read = 0;
    for path in &paths {
        bytes_read += std::fs::read_to_string(path)
            .map_err(|error| ScanError::Io(format!("reading {}: {error}", rel(root, path))))?
            .len();
        let is_workflow = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".workflow.js"));
        let mut command = std::process::Command::new("node");
        if is_workflow {
            command.arg("-e").arg(WORKFLOW_COMPILER).arg(path);
        } else {
            command.arg("--check").arg(path);
        }
        let output = command.output().map_err(|error| {
            ScanError::Io(format!("launching Node for {}: {error}", rel(root, path)))
        })?;
        if !output.status.success() {
            failures.insert(
                rel(root, path),
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            );
        }
    }

    Ok(ScriptParseReport {
        files_read: paths.len(),
        bytes_read,
        failures,
    })
}

fn scan_under(root: &Path, units: &[&Unit]) -> Result<ScanReport, ScanError> {
    let mut inputs = BTreeMap::new();
    let mut production_code: BTreeMap<String, String> = BTreeMap::new();
    let mut stats = InputStats::default();

    let rust_roots = production_roots(root);
    let mut rust_paths = BTreeSet::new();
    for_each_production_line(&rust_roots, &|_| false, &mut |path, code, _| {
        rust_paths.insert(path.to_path_buf());
        let code_for_file = production_code.entry(rel(root, path)).or_default();
        code_for_file.push_str(code);
        code_for_file.push('\n');
    });
    for path in rust_paths {
        read_input(root, &path, &mut inputs, &mut stats)?;
        stats.rust_files += 1;
    }

    let mut scripts = Vec::new();
    collect_matching_files(
        root,
        &root.join("scripts"),
        false,
        &|path| {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("js" | "mjs" | "cjs" | "py" | "sh")
            )
        },
        &mut scripts,
    )?;
    scripts.sort();
    for path in scripts {
        read_input(root, &path, &mut inputs, &mut stats)?;
        stats.script_files += 1;
    }

    let mut markdown = Vec::new();
    collect_matching_files(
        root,
        root,
        true,
        &|path| path.extension().and_then(|e| e.to_str()) == Some("md"),
        &mut markdown,
    )?;
    markdown.sort();
    for path in markdown {
        read_input(root, &path, &mut inputs, &mut stats)?;
        stats.markdown_files += 1;
    }

    if inputs.is_empty() {
        return Err(ScanError::NoInputs);
    }

    let mut markers = BTreeMap::new();
    for (path, text) in &inputs {
        for line in text.lines() {
            let Some((_, rest)) = line.split_once("INERT-CODE-RATCHET:") else {
                continue;
            };
            if let Some(id) = rest.split_whitespace().next() {
                *markers.entry((id.to_string(), path.clone())).or_default() += 1;
            }
        }
    }

    let mut findings = Vec::new();
    let mut incomplete = Vec::new();
    for unit in units {
        let marker = format!(
            "INERT-CODE-RATCHET: {} {}: {}",
            unit.id,
            unit.resolution.label(),
            unit.justification
        );
        let mut failures = Vec::new();
        let mut files = BTreeSet::new();
        for site in unit.sites {
            files.insert(site.file);
            let Some(text) = inputs.get(site.file) else {
                failures.push(format!("{} was outside the scan or unreadable", site.file));
                continue;
            };
            let searchable = if site.search_raw {
                text
            } else {
                production_code.get(site.file).unwrap_or(text)
            };
            let scoped_searchable;
            let searchable = if let Some(scope) = site.scope {
                let scope_occurrences = searchable.matches(scope).count();
                if scope_occurrences != 1 {
                    failures.push(format!(
                        "{} expected one scope {:?}, found {}",
                        site.file, scope, scope_occurrences
                    ));
                    continue;
                }
                let start = searchable.find(scope).expect("one scope occurrence");
                let after_scope = &searchable[start + scope.len()..];
                let end = after_scope
                    .find("\nimpl ")
                    .map_or(searchable.len(), |next| start + scope.len() + next);
                scoped_searchable = &searchable[start..end];
                scoped_searchable
            } else {
                searchable
            };
            let occurrences = searchable.matches(site.needle).count();
            if occurrences != site.occurrences {
                failures.push(format!(
                    "{} expected {} occurrence(s) of {:?}, found {}",
                    site.file, site.occurrences, site.needle, occurrences
                ));
            }
            match site.reason {
                ReasonRequirement::Marker => {
                    if !normalize_whitespace(text).contains(&normalize_whitespace(&marker)) {
                        failures.push(format!(
                            "{} lacks mandatory local reason {:?}",
                            site.file, marker
                        ));
                    }
                }
                ReasonRequirement::Existing(reason) => {
                    if !normalize_whitespace(text).contains(&normalize_whitespace(reason)) {
                        failures.push(format!(
                            "{} lacks mandatory local reason {:?}",
                            site.file, reason
                        ));
                    }
                }
            }
        }
        if failures.is_empty() {
            findings.push(Finding {
                id: unit.id,
                resolution: unit.resolution,
                files,
            });
        } else {
            incomplete.push(Incomplete {
                id: unit.id,
                failures,
            });
        }
    }

    Ok(ScanReport {
        stats,
        findings,
        incomplete,
        markers,
    })
}

fn expected_marker_identities(units: &[&Unit]) -> BTreeMap<(String, String), usize> {
    let mut expected = BTreeMap::new();
    for unit in units {
        for site in unit.sites {
            if matches!(site.reason, ReasonRequirement::Marker) {
                expected
                    .entry((unit.id.to_string(), site.file.to_string()))
                    .or_insert(1);
            }
        }
    }
    expected
}

fn all_units() -> Vec<&'static Unit> {
    DEBT.iter().chain(SANCTIONS).collect()
}

#[test]
fn the_named_inventory_only_decreases() {
    assert_eq!(
        DEBT.len(),
        KNOWN_INERT_UNITS,
        "the debt table itself must name exactly {KNOWN_INERT_UNITS} units"
    );
    assert_eq!(
        SANCTIONS.len(),
        KNOWN_SANCTIONS,
        "the sanctioned table itself must name exactly {KNOWN_SANCTIONS} units"
    );

    let units = all_units();
    let unique_ids = units.iter().map(|unit| unit.id).collect::<BTreeSet<_>>();
    assert_eq!(
        unique_ids.len(),
        units.len(),
        "every inert-code unit id must be unique"
    );
    assert!(
        units.iter().all(|unit| !unit.sites.is_empty()),
        "every inert-code unit must name at least one exact source site"
    );
    for unit in &units {
        assert!(!unit.id.trim().is_empty(), "an inert-code unit has no id");
        assert!(
            !unit.justification.trim().is_empty(),
            "{} has no justification",
            unit.id
        );
        for site in unit.sites {
            assert!(
                !site.file.trim().is_empty()
                    && !site.needle.trim().is_empty()
                    && site.occurrences > 0,
                "{} has an empty site specification: {site:?}",
                unit.id
            );
            match site.reason {
                ReasonRequirement::Marker => {}
                ReasonRequirement::Existing(reason) => {
                    assert!(
                        !reason.trim().is_empty(),
                        "{} has an empty reason: {site:?}",
                        unit.id
                    );
                }
            }
        }
    }
    assert_eq!(
        (
            DEBT.iter()
                .filter(|unit| unit.resolution == Resolution::Wire)
                .count(),
            DEBT.iter()
                .filter(|unit| unit.resolution == Resolution::Delete)
                .count(),
            DEBT.iter()
                .filter(|unit| unit.resolution == Resolution::Document)
                .count(),
        ),
        (25, 24, 2),
        "the accepted WIRE/DELETE/DOCUMENT classification changed"
    );
    assert!(
        SANCTIONS
            .iter()
            .all(|unit| unit.resolution == Resolution::Sanction),
        "only deliberate, locally justified inertness belongs in SANCTIONS"
    );

    let report = scan_under(&workspace_root(), &units).expect("scan the workspace");
    let incomplete_ids = report
        .incomplete
        .iter()
        .map(|unit| unit.id)
        .collect::<Vec<_>>();
    assert!(
        report.incomplete.is_empty(),
        "inert-code inventory lost a site or its local reason ({incomplete_ids:?}): {:#?}",
        report.incomplete,
    );

    let debt = report
        .findings
        .iter()
        .filter(|finding| finding.resolution != Resolution::Sanction)
        .collect::<Vec<_>>();
    assert!(
        debt.len() <= KNOWN_INERT_UNITS,
        "inert-code debt went UP: {} > {KNOWN_INERT_UNITS}: {debt:#?}",
        debt.len()
    );
    assert_eq!(
        debt.len(),
        KNOWN_INERT_UNITS,
        "inert-code debt went DOWN to {}. Lower KNOWN_INERT_UNITS and remove the paid unit in the same change.",
        debt.len()
    );

    let expected_ids = DEBT.iter().map(|unit| unit.id).collect::<BTreeSet<_>>();
    let actual_ids = debt
        .iter()
        .map(|finding| finding.id)
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_ids, expected_ids, "inert-code unit identity changed");
    assert_eq!(
        report.markers,
        expected_marker_identities(&units),
        "an inert-code marker moved, disappeared, or was added without an inventory row"
    );

    for finding in &report.findings {
        assert!(
            !finding.files.is_empty(),
            "{} names no source file",
            finding.id
        );
    }
}

#[test]
fn the_scan_reads_the_real_workspace_in_every_domain() {
    let units = all_units();
    let report = scan_under(&workspace_root(), &units).expect("scan the workspace");
    assert!(
        report.stats.rust_files >= 423,
        "Rust scan was narrowed: {:#?}",
        report.stats
    );
    assert!(
        report.stats.script_files >= 27,
        "script scan was narrowed: {:#?}",
        report.stats
    );
    assert!(
        report.stats.markdown_files >= 274,
        "Markdown scan was narrowed: {:#?}",
        report.stats
    );
    assert!(
        report.stats.bytes > 10_000_000,
        "scan read implausibly little input: {:#?}",
        report.stats
    );
}

#[test]
fn script_parse_failure_inventory_only_decreases() {
    let report = script_parse_failures_under(&workspace_root()).expect("parse script inputs");
    assert!(
        report.failures.len() <= KNOWN_SCRIPT_PARSE_FAILURES,
        "script parse failures went UP: {:#?}",
        report.failures
    );
    assert_eq!(
        report.failures.len(),
        KNOWN_SCRIPT_PARSE_FAILURES,
        "script parse failures went DOWN. Lower KNOWN_SCRIPT_PARSE_FAILURES and pay S01 in the same change."
    );
    assert_eq!(
        report
            .failures
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["scripts/eval/workflows/propose-verify.workflow.js"],
        "the script parse failure moved or was replaced"
    );
    assert!(
        report.files_read >= 5 && report.bytes_read > 10_000,
        "script parser read implausibly little input: {report:#?}"
    );
}

#[test]
fn the_script_detector_catches_a_synthetic_known_positive() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::create_dir_all(root.path().join("scripts")).unwrap();
    std::fs::write(
        root.path().join("scripts/valid.workflow.js"),
        "export const meta = {}\nconst ONE = 1\nreturn ONE\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("scripts/broken.workflow.js"),
        "export const meta = {}\nconst DUPLICATE = 1\nconst DUPLICATE = 2\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("scripts/valid.mjs"),
        "export const VALUE = 1\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("scripts/broken.js"),
        "const DUPLICATE = 1\nconst DUPLICATE = 2\n",
    )
    .unwrap();

    let report = script_parse_failures_under(root.path()).expect("parse synthetic scripts");
    assert_eq!(report.files_read, 4);
    assert_eq!(
        report
            .failures
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["scripts/broken.js", "scripts/broken.workflow.js"],
        "both parser branches must reject duplicate bindings and accept valid scripts"
    );
}

#[test]
fn registered_inventory_probe_reads_a_seeded_unit() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"probe\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("probe/src")).unwrap();
    std::fs::create_dir_all(root.path().join("scripts")).unwrap();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    let reason = "synthetic inert unit proves the production scan can detect.";
    let marker = format!("INERT-CODE-RATCHET: Z99 DELETE: {reason}");
    std::fs::write(
        root.path().join("probe/src/lib.rs"),
        format!("// {marker}\npub fn inert_seed() {{}}\n"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("scripts/probe.workflow.js"),
        format!("// {marker}\nconst DEAD = 1\n"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("docs/probe.md"),
        format!("<!-- {marker} -->\n# Probe\n"),
    )
    .unwrap();
    const SEED: Unit = unit!(
        "Z99",
        Delete,
        "synthetic inert unit proves the production scan can detect.",
        [
            site("probe/src/lib.rs", "pub fn inert_seed() {}"),
            site("scripts/probe.workflow.js", "const DEAD = 1"),
            site("docs/probe.md", "# Probe"),
        ]
    );
    let report = scan_under(root.path(), &[&SEED]).expect("scan seeded workspace");
    assert!(
        report.incomplete.is_empty(),
        "seed was incomplete: {:#?}",
        report.incomplete
    );
    assert_eq!(report.findings.len(), 1, "scanner missed the seeded unit");
    assert_eq!(report.findings[0].id, "Z99");
    assert_eq!(report.markers, expected_marker_identities(&[&SEED]));
}

#[test]
fn an_empty_root_is_a_hard_failure() {
    let root = tempfile::tempdir().expect("empty temporary root");
    let units = all_units();
    assert_eq!(
        scan_under(root.path(), &units).unwrap_err(),
        ScanError::NoInputs
    );
    assert_eq!(
        script_parse_failures_under(root.path()).unwrap_err(),
        ScanError::NoInputs
    );
}

#[test]
fn a_site_without_its_local_reason_is_not_counted() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"probe\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("probe/src")).unwrap();
    std::fs::write(
        root.path().join("probe/src/lib.rs"),
        "pub fn inert_seed() {}\n",
    )
    .unwrap();
    const SEED: Unit = unit!(
        "Z98",
        Delete,
        "this mandatory reason is absent.",
        [site("probe/src/lib.rs", "pub fn inert_seed() {}"),]
    );
    let report = scan_under(root.path(), &[&SEED]).expect("scan seed without reason");
    assert!(report.findings.is_empty());
    assert_eq!(report.incomplete.len(), 1);
    let missing_reason_was_reported = report.incomplete[0]
        .failures
        .iter()
        .any(|failure| failure.contains("mandatory local reason"));
    assert!(missing_reason_was_reported);
}

/// The twin of the test above, and the one that keeps the *site* detector
/// honest rather than only the *reason* detector.
///
/// Everything here is correct EXCEPT the registered needle: the file exists,
/// is readable, and carries its mandatory local marker. The only thing wrong
/// is that the code the row claims to indict is not there. A registry that
/// counted this as a finding would be pinning the marker rather than the
/// code, and would keep counting a unit whose declaration had been renamed or
/// deleted out from under it.
///
/// **This test exists to be killed by a mutation.** Delete the occurrence
/// check in `scan_under` and every other test in this file still passes; only
/// this one goes red. That was a real hole, found by adversarial review of the
/// PR that introduced this suite and proven by deleting the detector and
/// watching 7/7 stay green.
#[test]
fn a_site_whose_needle_is_absent_is_not_counted() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"probe\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("probe/src")).unwrap();
    let reason = "the marker is present but the code it indicts is gone.";
    let marker = format!("INERT-CODE-RATCHET: Z97 DELETE: {reason}");
    // Marker present, file readable — and the needle deliberately absent.
    std::fs::write(
        root.path().join("probe/src/lib.rs"),
        format!("// {marker}\npub fn something_else_entirely() {{}}\n"),
    )
    .unwrap();
    const SEED: Unit = unit!(
        "Z97",
        Delete,
        "the marker is present but the code it indicts is gone.",
        [site("probe/src/lib.rs", "pub fn inert_seed() {}"),]
    );
    let report = scan_under(root.path(), &[&SEED]).expect("scan seed without its needle");
    assert!(
        report.findings.is_empty(),
        "a unit whose registered needle is ABSENT was counted as a finding — \
         the site detector is not running, so this ratchet would pin markers \
         rather than code: {:#?}",
        report.findings
    );
    assert_eq!(
        report.incomplete.len(),
        1,
        "the absent needle was not reported"
    );
    let absent_needle_was_reported = report.incomplete[0]
        .failures
        .iter()
        .any(|failure| failure.contains("occurrence"));
    assert!(
        absent_needle_was_reported,
        "incomplete for the wrong reason — expected an occurrence-count \
         failure, got: {:#?}",
        report.incomplete[0].failures
    );
}
