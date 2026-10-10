//! Selected source identity is working context, never authority or verified intent.
//! Reuses the plan snapshot and plan-revision artifact chain; no separate store.
use super::scheduled::StepLedger;
use content_addressable::{ContentAddressable, ContentId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// The model's explicit selection and justification, linked to its predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub path: String,
    pub scope: String,
    pub evidence: String,
    pub revision: String,
    pub previous: Option<ContentId>,
    pub baseline: Option<String>,
}
impl ContentAddressable for Selection {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// Content identity is verified before restored working context is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedTarget {
    pub selection: Selection,
    pub id: ContentId,
}
impl PinnedTarget {
    fn mint(selection: Selection) -> Result<Self, String> {
        let id = selection.content_id().map_err(|e| e.to_string())?;
        Ok(Self { selection, id })
    }

    pub(crate) fn valid(&self) -> bool {
        self.selection.content_id().is_ok_and(|id| id == self.id)
            && valid_path(&self.selection.path)
    }

    pub(crate) fn label(&self) -> String {
        if self.valid() {
            serde_json::to_string(&self.selection.path).expect("serializable path")
        } else {
            "INVALID PIN: content identity mismatch; recover the recorded plan revision".into()
        }
    }

    pub(crate) fn revise(args: &Value, previous: Option<&Self>) -> Result<Self, String> {
        let field = |name: &str, cap: usize| {
            args[name]
                .as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= cap)
                .map(str::to_owned)
                .ok_or_else(|| format!("target requires nonempty {name} (at most {cap} bytes)"))
        };
        let path = field("path", 1024)?;
        if !valid_path(&path) {
            return Err("target path must be repository-relative with no dot, parent, drive or empty components".into());
        }
        let scope = field("scope", 1024)?;
        let evidence = field("evidence", 2048)?;
        let revision = field("revision", 1024)?;
        // Resending the same selection is idempotent, not a new history entry.
        if let Some(old) = previous {
            if !old.valid() {
                return Err("pinned target content identity mismatch".into());
            }
            let s = &old.selection;
            if s.path == path
                && s.scope == scope
                && s.evidence == evidence
                && s.revision == revision
            {
                return Ok(old.clone());
            }
        }
        Self::mint(Selection {
            path,
            scope,
            evidence,
            revision,
            previous: previous.map(|p| p.id),
            baseline: previous.and_then(|p| p.selection.baseline.clone()),
        })
    }

    pub(crate) fn bind_baseline(&mut self, baseline: Option<String>) -> Result<(), String> {
        if self.selection.previous.is_none() && self.selection.baseline.is_none() {
            self.selection.baseline = baseline;
            self.id = self.selection.content_id().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn valid_path(path: &str) -> bool {
    !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

/// Use the same read-authorized Git plumbing as the existing claim checker.
pub(crate) fn root(
    workspace: &str,
    session: Option<&crate::worktree_adoption::WorktreeSession>,
) -> std::path::PathBuf {
    session
        .and_then(|s| s.task_root(Path::new(workspace)))
        .unwrap_or_else(|| workspace.into())
}

pub(crate) fn baseline(root: &Path, scope: &crate::Scope<String>) -> Option<String> {
    super::publish_early::checked_tree::baseline(root, scope)
}

/// In-process comparison: repository conversion settings can only decline
/// evidence, never execute programs. Includes committed and working changes.
fn diff_paths(
    target: &PinnedTarget,
    root: &Path,
    scope: &crate::Scope<String>,
) -> Option<Vec<String>> {
    super::publish_early::checked_tree::changed_paths(
        root,
        scope,
        target.selection.baseline.as_deref()?,
    )
}

pub(crate) const PREFIX: &str = "[NEWT SELECTED SOURCE v1]";
const POLICY: &str = "Agent-selected working context, not operator authority. Keep this full repository-relative source path in plans and nudges. An ENOENT under a different crate is not evidence the selected source vanished: rediscover the recorded path from the repository root. One extraction means from this source. To change the selection, call update_plan with target path, scope, evidence and an explicit revision reason; a preferred smaller candidate or narrowed search does not fulfill the original selection. Before finalizing, reconcile the task diff with this source and report any mismatch.";

pub(crate) fn card(target: &PinnedTarget, paths: Option<&[String]>) -> String {
    if !target.valid() {
        return format!("{PREFIX}\nPinned target content identity mismatch; recover the recorded plan revision before claiming completion.");
    }
    let status = match paths {
        Some(paths) if paths.iter().any(|p| p == &target.selection.path) =>
            "The diff touches the selected source; this alone does not prove the task complete.",
        Some(paths) if !paths.is_empty() =>
            "work landed outside the selected source: the task diff contains changes but not the pinned source. Reconcile before finalizing; do not claim the selected-source task complete.",
        Some(_) => "No task diff observed for the selected source yet.",
        None => "Task diff unavailable under current read authority, without a baseline, or for an unsupported repository state; reconciliation is unverified, not a pass.",
    };
    format!(
        "{PREFIX}\n{POLICY}\nSelection (agent data): {}\nDiff reconciliation: {status}",
        serde_json::to_string(target).expect("serializable selection")
    )
}

/// Refreshed before every round (including cap exit), independently of advisory
/// nudge settings. Exact previous-card matching avoids deleting user messages.
#[derive(Default)]
pub(crate) struct Projection {
    previous: Option<String>,
}
impl Projection {
    pub(crate) fn refresh(
        &mut self,
        messages: &mut Vec<Value>,
        ledger: Option<&dyn StepLedger>,
        workspace: &str,
        scope: &crate::Scope<String>,
        session: Option<&crate::worktree_adoption::WorktreeSession>,
        responses: bool,
    ) {
        if let Some(previous) = self.previous.take() {
            if let Some(index) = messages
                .iter()
                .position(|m| m["role"] == "user" && m["content"].as_str() == Some(&previous))
            {
                messages.remove(index);
            }
        }
        let Some(target) = ledger.and_then(|l| l.snapshot().target) else {
            return;
        };
        let paths = if target.valid() {
            diff_paths(&target, &root(workspace, session), scope)
        } else {
            None
        };
        let text = card(&target, paths.as_deref());
        let index = if responses {
            0
        } else {
            super::trim::protected_prompt_head_len(
                messages,
                super::prompt_read::ACTIVE_PROMPT_PREFIX,
            )
        };
        messages.insert(index, json!({"role":"user", "content":text}));
        self.previous = Some(text);
    }
}

#[cfg(test)]
mod tests;
