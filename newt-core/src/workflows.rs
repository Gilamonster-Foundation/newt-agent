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

/// Front matter that lets the workflow classifier find this workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            }
        }
        out.push_str(
            "\nUse update_plan now to align the active plan to this workflow. Commit each verified coherent implementation step on the feature branch before pushing/opening the PR.",
        );
        out
    }
}

/// Built-in GitHub PR workflow: issue/request -> branch -> commits -> PR.
#[must_use]
pub fn builtin_workflows() -> Vec<WorkflowConfig> {
    vec![WorkflowConfig {
        name: "github_pr".to_string(),
        enabled: true,
        description: "read an issue/request, implement in tracked steps, commit to a branch, push, and open a GitHub PR".to_string(),
        classifier: WorkflowClassifierConfig {
            min_score: default_classifier_min_score(),
            keywords: vec![
                "github issue".to_string(),
                "github.com".to_string(),
                "pull request".to_string(),
                "open a pr".to_string(),
                "create a pr".to_string(),
            ],
            examples: vec![
                "Read this GitHub issue, plan the implementation, create a branch, commit the work, push, and open a PR.".to_string(),
                "Take a look at this issue, implement the fix in steps, commit each step, and create a pull request.".to_string(),
                "Build the requested change from an issue URL and get me a GitHub PR.".to_string(),
            ],
        },
        trigger_terms: Vec::new(),
        steps: vec![
            WorkflowStep {
                id: "read_issue".to_string(),
                title: "Read the issue/request and current repo state".to_string(),
                steer: "Gather ground truth from the issue, git status, branch, log, and relevant files before editing".to_string(),
            },
            WorkflowStep {
                id: "plan_implementation".to_string(),
                title: "Break the implementation into ordered steps".to_string(),
                steer: "Call update_plan with concrete implementation, verification, commit, push, and PR steps".to_string(),
            },
            WorkflowStep {
                id: "create_branch".to_string(),
                title: "Create or switch to a feature branch".to_string(),
                steer: "Use git status/branch ground truth before creating or switching branches".to_string(),
            },
            WorkflowStep {
                id: "implement_step".to_string(),
                title: "Implement the active step".to_string(),
                steer: "Make the smallest coherent edit for the active plan step".to_string(),
            },
            WorkflowStep {
                id: "verify_step".to_string(),
                title: "Verify the active step".to_string(),
                steer: "Run focused tests/checks that prove the step works".to_string(),
            },
            WorkflowStep {
                id: "commit_step".to_string(),
                title: "Commit the verified step to the branch".to_string(),
                steer: "Stage only relevant files and commit with the required LLM attribution trailer".to_string(),
            },
            WorkflowStep {
                id: "push_branch".to_string(),
                title: "Push the branch".to_string(),
                steer: "Push the feature branch after the intended commits are present".to_string(),
            },
            WorkflowStep {
                id: "open_pr".to_string(),
                title: "Open or update the GitHub PR".to_string(),
                steer: "Create/update the PR with what changed, test plan, and out-of-scope notes".to_string(),
            },
        ],
    }]
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
}
