//! Configurable workflow steers.
//!
//! Workflow drop-ins live under `~/.newt/workflows/` (or
//! `$NEWT_CONFIG_DIR/workflows/`). They are not runners: they are steerable
//! process definitions the agentic loop can quote when a model drifts into a
//! handoff summary instead of maintaining its plan and continuing.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const BUNDLED_GITHUB_PR_WORKFLOW: &str = include_str!("workflows/github_pr.toml");
const BUNDLED_DIAGNOSE_FAILURE_WORKFLOW: &str = include_str!("workflows/diagnose_failure.toml");
const BUNDLED_FIX_FAILING_TESTS_WORKFLOW: &str = include_str!("workflows/fix_failing_tests.toml");
const BUNDLED_RESEARCH_WORKFLOW: &str = include_str!("workflows/research.toml");
const BUNDLED_PLANNING_WORKFLOW: &str = include_str!("workflows/planning.toml");
const BUNDLED_TDD_PEER_REVIEW_WORKFLOW: &str = include_str!("workflows/tdd_peer_review.toml");

/// Front matter that lets the workflow classifier find this workflow.
///
/// `deny_unknown_fields`: a misplaced top-level field (e.g. `delegate_hint`
/// written AFTER `[classifier]` opens) would otherwise be silently parsed as
/// an unknown key of *this* table and dropped — no error, no warning, just a
/// field that quietly never takes effect. Denying unknown fields turns that
/// into a loud parse failure instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowClassifierConfig {
    /// Minimum prototype similarity required for an example match.
    #[serde(default = "default_classifier_min_score")]
    pub min_score: f32,
    /// Substrings that directly select this workflow.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Prototype task phrases for lightweight semantic matching.
    #[serde(default)]
    pub examples: Vec<String>,
}

impl Default for WorkflowClassifierConfig {
    fn default() -> Self {
        Self {
            min_score: default_classifier_min_score(),
            keywords: Vec::new(),
            examples: Vec::new(),
        }
    }
}

fn default_classifier_min_score() -> f32 {
    0.24
}

/// One workflow step the model should track in `update_plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Stable step key, e.g. `read_issue`.
    pub id: String,
    /// Short model-facing step title.
    pub title: String,
    /// Concrete steering text for this step.
    #[serde(default)]
    pub steer: String,
    /// Suggested tool names for this step (e.g. `["code_search", "read_file"]`,
    /// or `["crew"]`/`["team"]` when the step itself is naturally a
    /// delegation point). Advisory only — never a hard requirement, and
    /// never validated against what's actually available this session (a
    /// tool named here but not offered is simply not usable; the model
    /// already knows its real tool list). Empty means no specific
    /// suggestion for this step.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A configured workflow steer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Workflow name. For drop-ins this may be omitted; the filename stem wins.
    #[serde(default)]
    pub name: String,
    /// Whether this workflow is eligible for steering.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Short description shown in nudges.
    #[serde(default)]
    pub description: String,
    /// Classifier front matter for selecting this workflow.
    #[serde(default)]
    pub classifier: WorkflowClassifierConfig,
    /// Compatibility alias for early workflow configs. Prefer
    /// `[classifier].keywords` in new drop-ins.
    #[serde(default)]
    pub trigger_terms: Vec<String>,
    /// Ordered workflow steps.
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
    /// A short model-facing hint recommending `crew`/`team` sub-agent
    /// delegation instead of continuing to spend this session's own round
    /// budget, offered only when this workflow matches AND sub-agent dispatch
    /// is actually available this session. `None` (the default) means this
    /// workflow never suggests delegation.
    #[serde(default)]
    pub delegate_hint: Option<String>,
    /// Override for how many rounds without a plan/edit checkpoint still
    /// count as "recent progress" for round-cap grace, when this workflow
    /// matches. `None` uses the shared default. Diagnostic workflows
    /// legitimately need more read-only rounds between checkpoints than
    /// routine edits do.
    #[serde(default)]
    pub progress_horizon_rounds: Option<usize>,
}

fn default_enabled() -> bool {
    true
}

fn tokens(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter_map(|raw| {
            let t = raw.trim().to_ascii_lowercase();
            (t.len() >= 3).then_some(t)
        })
        .collect()
}

fn prototype_similarity(query: &BTreeSet<String>, prototype: &BTreeSet<String>) -> f32 {
    if query.is_empty() || prototype.is_empty() {
        return 0.0;
    }
    let overlap = query.intersection(prototype).count() as f32;
    let prototype_recall = overlap / prototype.len() as f32;
    let union = query.union(prototype).count() as f32;
    let jaccard = if union == 0.0 { 0.0 } else { overlap / union };
    jaccard.max(prototype_recall)
}

impl WorkflowConfig {
    fn with_name_from_path(mut self, path: &Path) -> Self {
        if self.name.trim().is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                self.name = stem.to_string();
            }
        }
        self
    }

    fn matches(&self, text: &str) -> bool {
        let lc = text.to_ascii_lowercase();
        let keyword_match = self
            .classifier
            .keywords
            .iter()
            .chain(self.trigger_terms.iter())
            .map(|term| term.trim().to_ascii_lowercase())
            .filter(|term| !term.is_empty())
            .any(|term| lc.contains(&term));
        if keyword_match {
            return true;
        }
        let query = tokens(text);
        self.classifier
            .examples
            .iter()
            .map(|example| prototype_similarity(&query, &tokens(example)))
            .fold(0.0_f32, f32::max)
            >= self.classifier.min_score
    }

    fn render_plan_update_hint(&self) -> String {
        let mut out = format!(
            "Configured workflow '{}' is active",
            if self.name.is_empty() {
                "unnamed"
            } else {
                &self.name
            }
        );
        if !self.description.trim().is_empty() {
            out.push_str(": ");
            out.push_str(self.description.trim());
        }
        out.push_str(". Track progress with update_plan so it survives session restarts/stops.");
        if !self.steps.is_empty() {
            out.push_str(" Workflow steps:");
            for step in self.steps.iter().take(12) {
                out.push_str("\n- ");
                out.push_str(&step.id);
                out.push_str(": ");
                out.push_str(step.title.trim());
                if !step.steer.trim().is_empty() {
                    out.push_str(" - ");
                    out.push_str(step.steer.trim());
                }
                if !step.tools.is_empty() {
                    out.push_str(" (tools: ");
                    out.push_str(&step.tools.join(", "));
                    out.push(')');
                }
            }
        }
        out.push_str(
            "\nUse update_plan now to align the active plan to this workflow. Commit each verified coherent implementation step on the feature branch before pushing/opening the PR.",
        );
        out
    }
}

/// Built-in workflows, most-specific first (a earlier entry wins ties in
/// [`WorkflowSteerer::matching`]'s first-match search):
/// - `fix_failing_tests` — the specific, mechanical run→fix→reverify LOOP.
/// - `tdd_peer_review` — test-first implementation with an independent review.
/// - `research` — an open-ended investigative question, not tied to a failure.
/// - `planning` — produce/critique a plan or design before implementing.
/// - `github_pr` — issue/request -> branch -> commits -> PR.
/// - `diagnose_failure` — the broadest: investigate a failing
///   test/pipeline/build/PR conflict, then land a fix.
///
/// All but `github_pr` carry a `delegate_hint` pointing at `crew`.
#[must_use]
pub fn builtin_workflows() -> Vec<WorkflowConfig> {
    vec![
        toml::from_str(BUNDLED_FIX_FAILING_TESTS_WORKFLOW)
            .expect("bundled fix-failing-tests workflow template is valid"),
        toml::from_str(BUNDLED_TDD_PEER_REVIEW_WORKFLOW)
            .expect("bundled TDD-peer-review workflow template is valid"),
        toml::from_str(BUNDLED_RESEARCH_WORKFLOW)
            .expect("bundled research workflow template is valid"),
        toml::from_str(BUNDLED_PLANNING_WORKFLOW)
            .expect("bundled planning workflow template is valid"),
        toml::from_str(BUNDLED_GITHUB_PR_WORKFLOW)
            .expect("bundled GitHub PR workflow template is valid"),
        toml::from_str(BUNDLED_DIAGNOSE_FAILURE_WORKFLOW)
            .expect("bundled diagnose-failure workflow template is valid"),
    ]
}

/// Load workflow drop-ins from a directory. Missing/malformed files are skipped.
#[must_use]
pub fn load_workflows_from_dir(dir: &Path) -> Vec<WorkflowConfig> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path).map(|s| toml::from_str::<WorkflowConfig>(&s)) {
            Ok(Ok(workflow)) => out.push(workflow.with_name_from_path(&path)),
            Ok(Err(e)) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping malformed workflow");
            }
            Err(_) => {}
        }
    }
    out
}

/// Merge workflow layers by name. Later layers replace earlier layers.
#[must_use]
pub fn merge_workflows(layers: Vec<Vec<WorkflowConfig>>) -> Vec<WorkflowConfig> {
    let mut order = Vec::new();
    let mut by_name: HashMap<String, WorkflowConfig> = HashMap::new();
    for layer in layers {
        for workflow in layer {
            if !by_name.contains_key(&workflow.name) {
                order.push(workflow.name.clone());
            }
            by_name.insert(workflow.name.clone(), workflow);
        }
    }
    order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect()
}

/// The workflow drop-in directory: `$NEWT_CONFIG_DIR/workflows` or
/// `~/.newt/workflows`.
#[must_use]
pub fn workflow_config_dir() -> Option<PathBuf> {
    Config::user_config_dir().map(|dir| dir.join("workflows"))
}

/// Configurable workflow steerer used by the agentic loop.
#[derive(Debug, Clone)]
pub struct WorkflowSteerer {
    workflows: Vec<WorkflowConfig>,
}

impl WorkflowSteerer {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            workflows: builtin_workflows(),
        }
    }

    #[must_use]
    pub fn from_workflows(workflows: Vec<WorkflowConfig>) -> Self {
        Self { workflows }
    }

    #[must_use]
    pub fn load_from_dir(dir: &Path) -> Self {
        Self::from_workflows(merge_workflows(vec![
            builtin_workflows(),
            load_workflows_from_dir(dir),
        ]))
    }

    /// Load user workflows from `~/.newt/workflows/`.
    #[must_use]
    pub fn load_default() -> Self {
        #[cfg(test)]
        {
            Self::builtin()
        }
        #[cfg(not(test))]
        {
            workflow_config_dir()
                .map(|dir| Self::load_from_dir(&dir))
                .unwrap_or_else(Self::builtin)
        }
    }

    /// Render a workflow hint for a plan-update stall.
    #[must_use]
    pub fn plan_update_hint(&self, text: &str) -> Option<String> {
        self.workflows
            .iter()
            .filter(|workflow| workflow.enabled)
            .find(|workflow| workflow.matches(text))
            .map(WorkflowConfig::render_plan_update_hint)
    }

    /// The FIRST enabled matching workflow, if any (shared by
    /// [`Self::delegate_hint`] and [`Self::progress_horizon`] so both read the
    /// same match rather than each re-running the classifier).
    fn matching(&self, text: &str) -> Option<&WorkflowConfig> {
        self.workflows
            .iter()
            .filter(|workflow| workflow.enabled)
            .find(|workflow| workflow.matches(text))
    }

    /// A short hint recommending `crew`/`team` delegation, when the matching
    /// workflow configures one AND sub-agent dispatch is available this
    /// session (`crew_available`). Never returned when `crew_available` is
    /// false, regardless of workflow match — there is nothing to delegate to.
    #[must_use]
    pub fn delegate_hint(&self, text: &str, crew_available: bool) -> Option<String> {
        if !crew_available {
            return None;
        }
        self.matching(text)?
            .delegate_hint
            .as_deref()
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_string)
    }

    /// The matching workflow's round-cap grace "recent progress" horizon
    /// override, if any.
    #[must_use]
    pub fn progress_horizon(&self, text: &str) -> Option<usize> {
        self.matching(text)?.progress_horizon_rounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_github_pr_workflow_mentions_branch_commit_push_and_pr() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("I need to plan the implementation and open a PR")
            .expect("built-in workflow should match");
        assert!(hint.contains("github_pr"), "{hint}");
        assert!(hint.contains("create_branch"), "{hint}");
        assert!(hint.contains("commit_step"), "{hint}");
        assert!(hint.contains("push_branch"), "{hint}");
        assert!(hint.contains("open_pr"), "{hint}");
        assert!(hint.contains("survives session restarts/stops"), "{hint}");
    }

    #[test]
    fn workflow_dropin_overrides_builtin_and_filename_supplies_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("github_pr.toml"),
            r#"
description = "custom PR flow"

[classifier]
keywords = ["ship it"]
examples = ["Ship this branch by committing the work and opening a PR."]

[[steps]]
id = "custom_step"
title = "Custom branch policy"
steer = "Always commit before pushing"
"#,
        )
        .unwrap();

        let steer = WorkflowSteerer::load_from_dir(dir.path());
        let hint = steer
            .plan_update_hint("ship it")
            .expect("drop-in workflow should match");
        assert!(hint.contains("custom PR flow"), "{hint}");
        assert!(hint.contains("custom_step"), "{hint}");
        assert!(!hint.contains("read_issue"), "{hint}");
    }

    #[test]
    fn step_tools_render_when_present_and_are_absent_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("synthetic.toml"),
            r#"
name = "synthetic"

[classifier]
keywords = ["do the synthetic thing"]

[[steps]]
id = "with_tools"
title = "A step naming tools"
steer = "do it"
tools = ["read_file", "crew"]

[[steps]]
id = "without_tools"
title = "A step naming no tools"
steer = "do the other thing"
"#,
        )
        .unwrap();

        let steer = WorkflowSteerer::load_from_dir(dir.path());
        let hint = steer
            .plan_update_hint("do the synthetic thing")
            .expect("synthetic workflow should match");
        assert!(hint.contains("(tools: read_file, crew)"), "{hint}");
        // The tool-less step's line must not carry a stray/empty "(tools: )".
        let without_tools_line = hint
            .lines()
            .find(|l| l.contains("without_tools"))
            .expect("without_tools step should render");
        assert!(
            !without_tools_line.contains("(tools:"),
            "{without_tools_line}"
        );
    }

    #[test]
    fn unmatched_text_does_not_select_a_default_workflow() {
        let steer = WorkflowSteerer::builtin();
        assert!(steer
            .plan_update_hint("Summarize the local cache eviction behavior")
            .is_none());
    }

    #[test]
    fn workflow_dir_is_under_user_config_dir() {
        let dir = workflow_config_dir().unwrap_or_else(|| PathBuf::from(".newt/workflows"));
        assert!(dir.ends_with("workflows"));
    }

    #[test]
    fn fix_failing_tests_workflow_matches_and_renders_tool_suggestions() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("Keep iterating on the tests until the whole suite is green.")
            .expect("built-in fix_failing_tests workflow should match");
        assert!(hint.contains("fix_failing_tests"), "{hint}");
        assert!(hint.contains("run_suite"), "{hint}");
        assert!(hint.contains("loop_or_stop"), "{hint}");
        // Per-step tool suggestions render into the hint.
        assert!(hint.contains("(tools: run_command"), "{hint}");
        assert!(hint.contains("edit_file"), "{hint}");
    }

    #[test]
    fn fix_failing_tests_delegate_hint_offers_crew_for_the_mechanical_loop() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .delegate_hint(
                "Fix all the failing tests one at a time and re-run until none fail.",
                true,
            )
            .expect("fix_failing_tests should match and offer a delegate hint");
        assert!(hint.contains("crew"), "{hint}");
    }

    #[test]
    fn research_workflow_matches_open_ended_investigation_not_a_failure() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint(
                "Investigate how these two subsystems communicate before we change either one.",
            )
            .expect("built-in research workflow should match");
        assert!(hint.contains("'research'"), "{hint}");
        assert!(hint.contains("gather_evidence"), "{hint}");
        assert!(hint.contains("(tools: code_search"), "{hint}");
    }

    #[test]
    fn research_delegate_hint_frames_crew_as_parallel_breadth() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .delegate_hint(
                "Explore the codebase to find every place this pattern is used.",
                true,
            )
            .expect("research should match and offer a delegate hint");
        assert!(hint.contains("crew"), "{hint}");
        assert!(hint.to_ascii_lowercase().contains("parallel"), "{hint}");
    }

    #[test]
    fn planning_workflow_matches_design_first_phrasing() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("Write a design doc proposing an approach to this problem.")
            .expect("built-in planning workflow should match");
        assert!(hint.contains("'planning'"), "{hint}");
        assert!(hint.contains("adversarial_check"), "{hint}");
        assert!(hint.contains("(tools: crew)"), "{hint}");
    }

    #[test]
    fn planning_does_not_steal_the_github_pr_workflows_own_match() {
        // Regression: "plan the implementation" is common enough vocabulary
        // that `planning`'s classifier (listed before `github_pr`) initially
        // stole this exact github_pr test phrase via example similarity.
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("I need to plan the implementation and open a PR")
            .expect("built-in workflow should match");
        assert!(hint.contains("github_pr"), "{hint}");
    }

    #[test]
    fn tdd_peer_review_workflow_matches_test_first_phrasing() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("Use TDD for this: write a failing test first, then implement.")
            .expect("built-in tdd_peer_review workflow should match");
        assert!(hint.contains("tdd_peer_review"), "{hint}");
        assert!(hint.contains("confirm_red"), "{hint}");
        assert!(hint.contains("peer_review"), "{hint}");
    }

    #[test]
    fn tdd_peer_review_delegate_hint_frames_crew_as_independent_reviewer() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .delegate_hint(
                "Build this test-first — red, green, refactor — and then peer review it.",
                true,
            )
            .expect("tdd_peer_review should match and offer a delegate hint");
        assert!(hint.contains("crew"), "{hint}");
        assert!(hint.to_ascii_lowercase().contains("independent"), "{hint}");
    }

    #[test]
    fn diagnose_failure_workflow_matches_failure_investigation_text() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .plan_update_hint("I'm showing broken tests here, can you find and fix them?")
            .expect("built-in diagnose_failure workflow should match");
        assert!(hint.contains("diagnose_failure"), "{hint}");
        assert!(hint.contains("reproduce"), "{hint}");
        assert!(hint.contains("isolate"), "{hint}");
    }

    #[test]
    fn delegate_hint_is_none_without_crew_available_even_on_a_match() {
        let steer = WorkflowSteerer::builtin();
        assert!(steer
            .delegate_hint("the pipeline is red, investigate why and fix it", false)
            .is_none());
    }

    #[test]
    fn delegate_hint_recommends_crew_team_when_matched_and_available() {
        let steer = WorkflowSteerer::builtin();
        let hint = steer
            .delegate_hint("the pipeline is red, investigate why and fix it", true)
            .expect("diagnose_failure workflow should match and offer a delegate hint");
        assert!(hint.contains("crew"), "{hint}");
        assert!(hint.contains("team"), "{hint}");
    }

    #[test]
    fn delegate_hint_is_none_for_a_workflow_without_one_configured() {
        // github_pr has no `delegate_hint` configured — a match must not
        // fabricate a delegation suggestion it didn't ask for.
        let steer = WorkflowSteerer::builtin();
        assert!(steer
            .delegate_hint("I need to plan the implementation and open a PR", true)
            .is_none());
    }

    #[test]
    fn delegate_hint_is_none_for_unmatched_text() {
        let steer = WorkflowSteerer::builtin();
        assert!(steer
            .delegate_hint("Summarize the local cache eviction behavior", true)
            .is_none());
    }

    #[test]
    fn progress_horizon_is_none_for_a_workflow_without_an_override() {
        let steer = WorkflowSteerer::builtin();
        assert_eq!(
            steer.progress_horizon("I need to plan the implementation and open a PR"),
            None
        );
    }

    #[test]
    fn progress_horizon_returns_the_matching_workflows_override() {
        let steer = WorkflowSteerer::builtin();
        assert_eq!(
            steer.progress_horizon("the pipeline is red, investigate why and fix it"),
            Some(6)
        );
    }

    /// Regression: a `delegate_hint`/`progress_horizon_rounds` field written
    /// AFTER `[classifier]` opens used to be silently swallowed as an unknown
    /// key of `WorkflowClassifierConfig` — no error, and the field just never
    /// took effect (the exact bug this bundled workflow file hit while being
    /// authored). `deny_unknown_fields` turns that into a loud parse error.
    #[test]
    fn misplaced_top_level_field_after_classifier_is_a_parse_error_not_silently_dropped() {
        let toml = r#"
name = "bad"

[classifier]
keywords = ["x"]
delegate_hint = "this belongs to WorkflowConfig, not WorkflowClassifierConfig"
"#;
        let err = toml::from_str::<WorkflowConfig>(toml)
            .expect_err("a misplaced field must fail to parse, not silently vanish");
        assert!(
            err.to_string().to_ascii_lowercase().contains("unknown"),
            "{err}"
        );
    }

    #[test]
    fn progress_horizon_is_none_for_unmatched_text() {
        let steer = WorkflowSteerer::builtin();
        assert_eq!(
            steer.progress_horizon("Summarize the local cache eviction behavior"),
            None
        );
    }
}
