//! Objective-scoped, content-addressed facts. Model prose is not evidence.
mod capture;
mod files;
mod prose;
pub(crate) mod review;
pub use prose::assistant_prose;
pub(super) use prose::model_prose;
pub(crate) use prose::{replay_messages, split_report};
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
    review: Option<review::Review>,
}

#[derive(Debug)]
struct Root {
    baseline: Option<files::Snapshot>,
    checks: Vec<Check>,
    publications: Vec<String>,
    limited: bool,
    notices: Vec<String>,
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
    notices: &'a [String],
}
impl ContentAddressable for Report<'_> {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

const NOTICES_HEADING: &str = "Last turn harness notices (historical, not assistant-authored):";

const FILES_HEADING: &str = "Files since objective/adoption snapshot (LF counts; absence is 0):";
const CHECKS_HEADING: &str = "Last observed checks per exact command/cwd (historical observations, not current-tree certification):";
const PUBLICATIONS_HEADING: &str =
    "Governed publication observations (not a live remote-state check):";

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
                notices: Vec::new(),
            },
        );
    }

    pub(crate) fn set_notices(&mut self, root: &Path, notices: &[String]) {
        if let Some(state) = self.roots.get_mut(root) {
            const MARKER: &str = "\n[Harness notice excerpt: retention limit reached.]";
            state.notices = notices
                .iter()
                .filter(|s| !s.trim().is_empty())
                .take(8)
                .map(|s| {
                    if s.chars().count() > 8192 {
                        format!("{}{MARKER}", capture::bounded(s, 8192 - MARKER.len()))
                    } else {
                        capture::bounded(s, 8192)
                    }
                })
                .collect();
            if notices.iter().filter(|s| !s.trim().is_empty()).count() > 8 {
                state.notices[7] = "Additional harness notices omitted by retention limits.".into();
            }
        }
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
        self.review = Some(review::review(
            prose,
            review::Facts {
                before: None,
                after: None,
                checks: &[],
                publications: &[],
                root,
            },
        ));
        let Some(state) = self.roots.get(root) else {
            return "## Observed\n\nFacts unavailable: root retention limit reached.\n\n## Model explanation\n\n".into();
        };
        let current = files::snapshot_with_baseline(root, scope, state.baseline.as_ref());
        self.review = Some(review::review(
            prose,
            review::Facts {
                before: state.baseline.as_ref(),
                after: current.as_ref(),
                checks: &state.checks,
                publications: &state.publications,
                root,
            },
        ));
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
            schema: "newt.observed-report/v2",
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
            notices: &state.notices,
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
        out.push_str(FILES_HEADING);
        out.push('\n');
        if report.files.is_empty() {
            out.push_str("- No content changes observed.\n");
        }
        for row in &report.files {
            out.push_str(&format!("- {row}\n"));
        }
        out.push('\n');
        out.push_str(CHECKS_HEADING);
        out.push('\n');
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
        out.push('\n');
        out.push_str(PUBLICATIONS_HEADING);
        out.push('\n');
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
        if !report.notices.is_empty() {
            out.push('\n');
            out.push_str(NOTICES_HEADING);
            out.push('\n');
            for line in report
                .notices
                .iter()
                .flat_map(|s| s.lines())
                .filter(|s| !s.trim().is_empty())
            {
                out.push_str(&format!(
                    "- {}\n",
                    crate::worktree_adoption::task_path_literal(Path::new(line))
                ));
            }
        }
        out.push_str("\n## Model explanation\n\n");
        out
    }
}

fn literal(text: &str) -> String {
    crate::worktree_adoption::task_path_literal(Path::new(&capture::bounded(text, 2048)))
}
