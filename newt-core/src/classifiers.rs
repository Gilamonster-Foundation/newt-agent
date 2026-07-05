//! Configurable lightweight classifiers.
//!
//! Classifier drop-ins live under `~/.newt/classifiers/` (or
//! `$NEWT_CONFIG_DIR/classifiers/`). The first shipped classifier is the
//! `NudgeClassifier`, used by the agentic loop to distinguish final answers
//! from no-tool-call stall shapes such as "I am about to continue" narrations
//! and stale-plan findings summaries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const NUDGE_CLASSIFIER_FILE: &str = "nudge.toml";
const BUNDLED_NUDGE_CLASSIFIER: &str = include_str!("classifiers/nudge.toml");

/// A nudge class the agentic loop knows how to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeClass {
    /// The assistant is announcing an action it has not actually performed.
    PendingAction,
    /// The assistant discovered prerequisite work that should revise the plan.
    PlanUpdate,
    /// The assistant appears to be giving a real final answer.
    FinalAnswer,
    /// No configured class was close enough to trust.
    Unknown,
}

/// Result of a nudge classifier pass.
#[derive(Debug, Clone, PartialEq)]
pub struct NudgeClassification {
    pub class: NudgeClass,
    pub score: f32,
}

impl NudgeClassification {
    pub fn is_pending_action(&self) -> bool {
        matches!(
            self.class,
            NudgeClass::PendingAction | NudgeClass::PlanUpdate
        )
    }

    pub fn is_plan_update(&self) -> bool {
        self.class == NudgeClass::PlanUpdate
    }
}

/// Config file shape for `~/.newt/classifiers/nudge.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NudgeClassifierConfig {
    /// Schema version. Currently informational; future versions can migrate.
    #[serde(default = "default_classifier_version")]
    pub version: u32,
    /// Minimum similarity score required before a class is trusted.
    #[serde(default = "default_min_score")]
    pub min_score: f32,
    /// Minimum gap between the winning class and runner-up.
    #[serde(default = "default_min_margin")]
    pub min_margin: f32,
    /// Class definitions keyed by canonical class names:
    /// `pending_action`, `plan_update`, `final_answer`.
    #[serde(default)]
    pub classes: BTreeMap<String, NudgeClassConfig>,
}

/// One nudge class: many input matchers share one output nudge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NudgeClassConfig {
    /// Prototype phrases that classify model output into this class.
    #[serde(default)]
    pub matchers: Vec<String>,
    /// Model-facing corrective direction sent when this class triggers an
    /// auto-continue path. Empty means "do not send a class-specific nudge".
    #[serde(default)]
    pub nudge: String,
}

impl Default for NudgeClassifierConfig {
    fn default() -> Self {
        bundled_nudge_config()
    }
}

fn default_classifier_version() -> u32 {
    1
}

fn default_min_score() -> f32 {
    0.28
}

fn default_min_margin() -> f32 {
    0.03
}

fn bundled_nudge_config() -> NudgeClassifierConfig {
    toml::from_str(BUNDLED_NUDGE_CLASSIFIER).expect("bundled nudge classifier template is valid")
}

fn bundled_nudge_classes() -> BTreeMap<String, NudgeClassConfig> {
    bundled_nudge_config().classes
}

impl NudgeClassifierConfig {
    /// Load only this config file. Missing files are treated as defaults; malformed
    /// files return an error so callers that explicitly asked to load can surface it.
    pub fn load_file(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Self = toml::from_str(&text)?;
                Ok(cfg.with_builtin_fallbacks())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Load from a classifier directory, e.g. `~/.newt/classifiers`.
    pub fn load_from_dir(dir: &Path) -> anyhow::Result<Self> {
        Self::load_file(&dir.join(NUDGE_CLASSIFIER_FILE))
    }

    fn with_builtin_fallbacks(mut self) -> Self {
        let builtins = bundled_nudge_classes();
        for (class, bundled) in builtins {
            self.classes
                .entry(class)
                .and_modify(|configured| {
                    if configured.matchers.is_empty() {
                        configured.matchers = bundled.matchers.clone();
                    }
                    if configured.nudge.trim().is_empty() {
                        configured.nudge = bundled.nudge.clone();
                    }
                })
                .or_insert(bundled);
        }
        if self.min_score <= 0.0 {
            self.min_score = default_min_score();
        }
        if self.min_margin < 0.0 {
            self.min_margin = default_min_margin();
        }
        self
    }
}

/// Configurable classifier for no-tool-call nudge decisions.
#[derive(Debug, Clone)]
pub struct NudgeClassifier {
    cfg: NudgeClassifierConfig,
}

impl Default for NudgeClassifier {
    fn default() -> Self {
        Self::builtin()
    }
}

impl NudgeClassifier {
    pub fn builtin() -> Self {
        Self {
            cfg: NudgeClassifierConfig::default(),
        }
    }

    pub fn from_config(cfg: NudgeClassifierConfig) -> Self {
        Self {
            cfg: cfg.with_builtin_fallbacks(),
        }
    }

    pub fn load_from_dir(dir: &Path) -> anyhow::Result<Self> {
        Ok(Self::from_config(NudgeClassifierConfig::load_from_dir(
            dir,
        )?))
    }

    /// Load the user nudge classifier from `~/.newt/classifiers/nudge.toml`.
    /// Unit tests use built-ins by default so local user config cannot make the
    /// harness tests nondeterministic; explicit load_from_dir tests still cover
    /// the file format.
    pub fn load_default() -> Self {
        #[cfg(test)]
        {
            Self::builtin()
        }
        #[cfg(not(test))]
        {
            let Some(dir) = classifier_config_dir() else {
                return Self::builtin();
            };
            match Self::load_from_dir(&dir) {
                Ok(classifier) => classifier,
                Err(e) => {
                    tracing::warn!(
                        path = %dir.join(NUDGE_CLASSIFIER_FILE).display(),
                        error = %e,
                        "failed to load nudge classifier config; using built-ins"
                    );
                    Self::builtin()
                }
            }
        }
    }

    pub fn classify(&self, text: &str) -> NudgeClassification {
        let query = tokens(text);
        if query.is_empty() {
            return NudgeClassification {
                class: NudgeClass::Unknown,
                score: 0.0,
            };
        }

        let mut scored: Vec<(NudgeClass, f32)> = self
            .cfg
            .classes
            .iter()
            .filter_map(|(class, class_cfg)| {
                let class = parse_nudge_class(class)?;
                let best = class_cfg
                    .matchers
                    .iter()
                    .map(|example| prototype_similarity(&query, &tokens(example)))
                    .fold(0.0_f32, f32::max);
                Some((class, best))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let Some((class, score)) = scored.first().copied() else {
            return NudgeClassification {
                class: NudgeClass::Unknown,
                score: 0.0,
            };
        };
        let runner_up = scored.get(1).map(|(_, s)| *s).unwrap_or(0.0);
        if score < self.cfg.min_score || (score - runner_up) < self.cfg.min_margin {
            return NudgeClassification {
                class: NudgeClass::Unknown,
                score,
            };
        }
        NudgeClassification { class, score }
    }

    pub fn is_pending_action(&self, text: &str) -> bool {
        self.classify(text).is_pending_action()
    }

    pub fn direction_for(&self, class: NudgeClass) -> Option<&str> {
        self.cfg
            .classes
            .get(nudge_class_key(class))
            .map(|class_cfg| class_cfg.nudge.as_str())
            .map(str::trim)
            .filter(|direction| !direction.is_empty())
    }
}

/// The classifier config root: `$NEWT_CONFIG_DIR/classifiers` or
/// `~/.newt/classifiers`.
pub fn classifier_config_dir() -> Option<PathBuf> {
    Config::user_config_dir().map(|dir| dir.join("classifiers"))
}

fn parse_nudge_class(s: &str) -> Option<NudgeClass> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "pending_action" | "continue" | "continuation" => Some(NudgeClass::PendingAction),
        "plan_update" | "update_plan" | "stale_plan" | "replan" => Some(NudgeClass::PlanUpdate),
        "final_answer" | "final" | "done" => Some(NudgeClass::FinalAnswer),
        _ => None,
    }
}

fn nudge_class_key(class: NudgeClass) -> &'static str {
    match class {
        NudgeClass::PendingAction => "pending_action",
        NudgeClass::PlanUpdate => "plan_update",
        NudgeClass::FinalAnswer => "final_answer",
        NudgeClass::Unknown => "unknown",
    }
}

fn tokens(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter_map(|raw| {
            let t = raw.trim().to_ascii_lowercase();
            (t.len() >= 3).then_some(t)
        })
        .collect()
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn prototype_similarity(query: &BTreeSet<String>, prototype: &BTreeSet<String>) -> f32 {
    if query.is_empty() || prototype.is_empty() {
        return 0.0;
    }
    let overlap = query.intersection(prototype).count() as f32;
    let prototype_recall = overlap / prototype.len() as f32;
    jaccard(query, prototype).max(prototype_recall)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_classifier_builtin_matches_known_phrases() {
        let classifier = NudgeClassifier::builtin();
        assert!(classifier.is_pending_action(
            "Plan is current — no update needed. Continuing with step 2: inserting the progressive dispatch into lib.rs."
        ));
        assert!(classifier.is_pending_action(
            "\
Summary

Progress so far: Step 2 is in progress.

Current blocker: The for loop needs to iterate over the slice directly.
The fix is to use for entry in page.entries.iter().

Next steps needed:
1. Fix the iteration type error in lib.rs.
2. Add tests.
3. Run just check."
        ));
        assert!(classifier.is_pending_action("Now I'll add the --home flag to the Cli struct."));
        assert!(classifier.is_pending_action("Let me keep editing now."));
        assert!(classifier.is_pending_action(
            "I found the issue - there's an extra closing brace } on line 809 of help_sections.rs that's causing a syntax error. I need to remove this stray brace."
        ));
        assert!(classifier.is_pending_action(
            "I have two issues: duplicate topic_has_rollups and a stray brace. Let me fix both — read around 490 to see what needs removing, then verify with a build check."
        ));
        assert!(!classifier.is_pending_action("The capital of France is Paris."));
        assert!(!classifier.is_pending_action("Done. Let me know if you want any further changes."));
        assert!(!classifier.is_pending_action(
            "The duplicate helper definitions and stray brace were removed, and the build check passed."
        ));
        assert!(
            classifier
                .direction_for(NudgeClass::PendingAction)
                .is_some_and(|d| d.contains("emit the tool call now")),
            "bundled classifier should carry output direction text"
        );
    }

    #[test]
    fn nudge_classifier_separates_plan_update_summaries_from_final_summaries() {
        let classifier = NudgeClassifier::builtin();
        let findings = "\
Summary of Findings

Across the tool calls, I observed two issues in newt-tui/src/help_sections.rs:
1. Duplicate function
2. Stray closing brace

Current Status

The build is broken due to these syntax errors. The plan was at step 2, but we need to fix the immediate compilation issues first.

Next Steps Required

To continue, I would need to remove the duplicate function using edit_file, remove the stray brace using edit_file, verify cargo check, then proceed with step 2 of the plan.

However, I've reached the tool-call limit and cannot make these edits now.";

        let classified = classifier.classify(findings);
        assert_eq!(classified.class, NudgeClass::PlanUpdate);
        assert!(classified.is_pending_action());
        assert!(classified.is_plan_update());

        let resume_handoff = "\
Summary

I reached the tool-call limit. Current state of newt-tui/src/help_sections.rs:
duplicate topic_has_rollups and rollup_page_for_topic definitions need to be removed.

Recommended next action if session resumes:
1. Fix the duplicate functions.
2. Clean up the broken test block.
3. Read lib.rs and wire the progressive dispatch.

The build is currently broken due to the duplicate definitions — that's the blocker for any further progress.";
        assert_eq!(
            classifier.classify(resume_handoff).class,
            NudgeClass::PlanUpdate
        );

        let final_summary =
            classifier.classify("Here is a summary of what I found across the tool calls.");
        assert_eq!(final_summary.class, NudgeClass::FinalAnswer);
    }

    #[test]
    fn nudge_classifier_loads_dropin_from_classifiers_dir() {
        let dir = tempfile::tempdir().unwrap();
        let classifiers = dir.path().join("classifiers");
        std::fs::create_dir_all(&classifiers).unwrap();
        std::fs::write(
            classifiers.join(NUDGE_CLASSIFIER_FILE),
            r#"
version = 1
min_score = 0.20
min_margin = 0.01

[classes.pending_action]
matchers = ["Proceeding with the patch by editing the target file."]
nudge = "Call the edit tool now."

[classes.final_answer]
matchers = ["No changes are needed."]
"#,
        )
        .unwrap();

        let classifier = NudgeClassifier::load_from_dir(&classifiers).unwrap();
        assert!(
            classifier.is_pending_action("Proceeding with the patch by editing the target file.")
        );
        assert!(!classifier.is_pending_action("No changes are needed."));
        assert_eq!(
            classifier.direction_for(NudgeClass::PendingAction),
            Some("Call the edit tool now.")
        );
        assert!(
            classifier
                .direction_for(NudgeClass::PlanUpdate)
                .is_some_and(|d| d.contains("Update the plan first")),
            "partial user configs still inherit bundled nudges"
        );
    }

    #[test]
    fn classifier_dir_is_under_user_config_dir() {
        let dir = classifier_config_dir().unwrap_or_else(|| PathBuf::from(".newt/classifiers"));
        assert!(dir.ends_with("classifiers"));
    }
}
