//! Objective-scoped, content-addressed facts. Model prose is not evidence.
mod capture;
mod files;
mod prose;
pub(super) use prose::model_prose;
#[cfg(test)]
pub(crate) mod tests;

use crate::{ExecOutcome, Scope};
pub(crate) use capture::{Capture, Presentation};
use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_ROOTS: usize = 4;
const MAX_CHECKS: usize = 16;
const MAX_PUBLICATIONS: usize = 16;

#[derive(Debug, Default)]
pub(crate) struct State {
    roots: BTreeMap<PathBuf, Root>,
    objective: Option<crate::prompt::PromptId>,
    /// Canonical bytes of the latest pinned report; replaced, never mistaken for history.
    pinned: Option<(ContentId, Vec<u8>)>,
}

#[derive(Debug)]
struct Root {
    baseline: Option<files::Snapshot>,
    checks: Vec<Check>,
    publications: Vec<String>,
    limited: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Check {
    command: String,
    cwd: String,
    outcome: String,
    exit: Option<i64>,
    lines: Vec<String>,
    truncated: bool,
}

/// The first harness-owned report item. CID covers scope, baseline, facts and
/// availability, not the model explanation. Paths/URLs remain locators.
#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    objective: Option<crate::prompt::PromptId>,
    root: String,
    baseline: Option<ContentId>,
    current: Option<ContentId>,
    files: Vec<String>,
    ambiguous: Vec<String>,
    checks: &'a [Check],
    publications: &'a [String],
    limited: bool,
}
impl ContentAddressable for Report<'_> {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

impl State {
    pub(crate) fn begin(&mut self, objective: crate::prompt::PromptId) {
        if self.objective != Some(objective) {
            *self = Self {
                objective: Some(objective),
                ..Self::default()
            };
        }
    }

    pub(crate) fn bind(&mut self, root: &Path, scope: &Scope<String>) {
        if self.roots.contains_key(root) || self.roots.len() >= MAX_ROOTS {
            return;
        }
        self.roots.insert(
            root.into(),
            Root {
                baseline: files::snapshot(root, scope),
                checks: Vec::new(),
                publications: Vec::new(),
                limited: false,
            },
        );
    }

    pub(crate) fn observe(
        &mut self,
        root: &Path,
        capture: &Capture,
        outcome: Option<ExecOutcome>,
        publication: Option<&crate::git_staging::Outcome>,
    ) {
        let Some(state) = self.roots.get_mut(root) else {
            return;
        };
        if let Some((command, cwd)) = &capture.command {
            let check = Check {
                command: command.clone(),
                cwd: cwd.clone(),
                outcome: format!("{:?}", outcome.unwrap_or(ExecOutcome::Unavailable)),
                exit: capture.exit,
                lines: capture.lines.clone(),
                truncated: capture.truncated,
            };
            state
                .checks
                .retain(|old| old.command != check.command || old.cwd != check.cwd);
            if state.checks.len() == MAX_CHECKS {
                state.checks.remove(0);
                state.limited = true;
            }
            state.checks.push(check);
        }
        let fact = match publication {
            Some(crate::git_staging::Outcome::Pushed {
                oid,
                owner,
                name,
                branch,
            }) => Some(format!(
                "Push observed: {owner}/{name}, branch {branch}, revision {oid}"
            )),
            Some(crate::git_staging::Outcome::PrCreated { url }) => {
                crate::git_staging::validate_pr_url(url)
                    .map(|url| format!("PR creation observed: {url}"))
            }
            _ => None,
        };
        if let Some(fact) = fact {
            if !state.publications.contains(&fact) {
                if state.publications.len() == MAX_PUBLICATIONS {
                    state.publications.remove(0);
                    state.limited = true;
                }
                state.publications.push(fact);
            }
        }
    }

    pub(crate) fn render(&mut self, root: &Path, scope: &Scope<String>, prose: &str) -> String {
        self.pinned = None;
        let Some(state) = self.roots.get(root) else {
            return "## Observed\n\nFacts unavailable: root retention limit reached.\n\n## Model explanation\n\n".into();
        };
        let current = files::snapshot_with_baseline(root, scope, state.baseline.as_ref());
        let rows = match (&state.baseline, &current) {
            (Some(before), Some(after)) => files::changes(before, after),
            _ => vec!["File counts unavailable: incomplete or unauthorized snapshot.".into()],
        };
        let mut rows = rows;
        if rows.len() > 64 {
            let omitted = rows.len() - 64;
            rows.truncate(64);
            rows.push(format!("{omitted} additional file rows omitted."));
        }
        let report = Report {
            schema: "newt.observed-report/v1",
            objective: self.objective,
            root: root.to_string_lossy().into_owned(),
            baseline: state.baseline.as_ref().and_then(|s| s.content_id().ok()),
            current: current.as_ref().and_then(|s| s.content_id().ok()),
            ambiguous: current
                .as_ref()
                .map_or_else(Vec::new, |s| files::ambiguous(s, prose)),
            files: rows,
            checks: &state.checks,
            publications: &state.publications,
            limited: state.limited,
        };
        let Ok(bytes) = report.canonical_form() else {
            return "## Observed\n\nFacts unavailable: content encoding failed.\n\n## Model explanation\n\n".into();
        };
        self.pinned = Some((ContentId::from_canonical_bytes(&bytes), bytes));
        let (cid, _) = self.pinned.as_ref().expect("report just pinned");
        let mut out = format!(
            "## Observed\n\nReport `{cid}` — root {}.\n\n",
            crate::worktree_adoption::task_path_literal(root)
        );
        out.push_str("Files since objective/adoption snapshot (LF counts; absence is 0):\n");
        if report.files.is_empty() {
            out.push_str("- No content changes observed.\n");
        }
        for row in &report.files {
            out.push_str(&format!("- {row}\n"));
        }
        out.push_str("\nLast observed checks per exact command/cwd (historical observations, not current-tree certification):\n");
        if state.checks.is_empty() {
            out.push_str("- No check observation available.\n");
        }
        for check in &state.checks {
            out.push_str(&format!(
                "- {} in {}: {}; exit {}.\n",
                literal(&check.command),
                literal(&check.cwd),
                check.outcome,
                check
                    .exit
                    .map_or_else(|| "unavailable".into(), |v| v.to_string())
            ));
            for line in &check.lines {
                out.push_str(&format!("  {}\n", literal(line)));
            }
            if check.truncated {
                out.push_str("  Result excerpt incomplete; totals unavailable.\n");
            }
        }
        out.push_str("\nGoverned publication observations (not a live remote-state check):\n");
        if state.publications.is_empty() {
            out.push_str("- No governed receipt available.\n");
        }
        for fact in &state.publications {
            out.push_str(&format!("- {}\n", literal(fact)));
        }
        if state.limited {
            out.push_str("\nOlder observations omitted by retention limits.\n");
        }
        if !report.ambiguous.is_empty() {
            out.push_str(&format!(
                "\nUnverified ambiguous bare paths in model explanation: {}.\n",
                report.ambiguous.join(", ")
            ));
        }
        out.push_str("\n## Model explanation\n\n");
        out
    }
}

fn literal(text: &str) -> String {
    crate::worktree_adoption::task_path_literal(Path::new(&capture::bounded(text, 2048)))
}
