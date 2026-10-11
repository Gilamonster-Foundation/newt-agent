//! Turn-local claim-check routing and HEAD baseline ownership (#2787).
use super::*;
use crate::worktree_adoption::WorktreeSession;
use std::path::{Path, PathBuf};

/// The baseline belongs to one checkout. A session can bind or lift a task
/// during this turn, so resolve its current root again at finalization.
pub(crate) struct TurnClaims<'a> {
    session: Option<&'a WorktreeSession>,
    baseline_root: PathBuf,
    head_before: Option<String>,
    evidence: super::super::observed_report::review::Evidence<'a>,
    report: std::sync::Arc<std::sync::Mutex<super::super::observed_report::State>>,
}

fn root(workspace: &str, session: Option<&WorktreeSession>) -> PathBuf {
    session
        .and_then(|session| session.task_root(Path::new(workspace)))
        .unwrap_or_else(|| PathBuf::from(workspace))
}

impl<'a> TurnClaims<'a> {
    pub(crate) fn capture(
        workspace: &str,
        read_scope: &crate::Scope<String>,
        session: Option<&'a WorktreeSession>,
    ) -> Self {
        let baseline_root = root(workspace, session);
        let head_before = git_head(&baseline_root.to_string_lossy(), read_scope);
        let report = session.map(|s| s.report.clone()).unwrap_or_default();
        report
            .lock()
            .expect("report state")
            .bind(&baseline_root, read_scope);
        Self {
            report,
            session,
            baseline_root,
            head_before,
            evidence: Default::default(),
        }
    }

    pub(crate) fn with_evidence(
        mut self,
        evidence: super::super::observed_report::review::Evidence<'a>,
    ) -> Self {
        self.evidence = evidence;
        self
    }

    pub(crate) fn final_report(
        &self,
        workspace: &str,
        scope: &crate::Scope<String>,
        prose: &str,
        source_draft: &str,
        disclosure: Option<&crate::ocap::DisclosureFilter>,
    ) -> String {
        let root = root(workspace, self.session);
        self.report.lock().expect("report state").compose(
            &root,
            scope,
            prose,
            source_draft,
            self.evidence,
            disclosure,
        )
    }

    pub(crate) fn observe_report(
        &self,
        workspace: &str,
        scope: &crate::Scope<String>,
        capture: &super::super::observed_report::Capture,
        outcome: Option<crate::ExecOutcome>,
        publication: Option<&crate::git_staging::Outcome>,
    ) {
        let root = root(workspace, self.session);
        let mut report = self.report.lock().expect("report state");
        report.bind(&root, scope);
        report.observe(&root, scope, capture, outcome, publication);
    }

    pub(crate) fn set_notices(
        &self,
        workspace: &str,
        scope: &crate::Scope<String>,
        notices: &[String],
    ) {
        let root = root(workspace, self.session);
        let mut report = self.report.lock().expect("report state");
        report.bind(&root, scope);
        report.set_notices(&root, notices);
    }

    pub(crate) fn observed_report(
        &self,
        workspace: &str,
        scope: &crate::Scope<String>,
        prose: &str,
    ) -> String {
        let root = root(workspace, self.session);
        let mut report = self.report.lock().expect("report state");
        report.bind(&root, scope);
        report.render(&root, scope, prose)
    }

    pub(crate) fn annotate(
        &self,
        text: String,
        workspace: &str,
        directories: &[PathBuf],
        read_scope: &crate::Scope<String>,
    ) -> String {
        let root = root(workspace, self.session);
        let root_changed = root != self.baseline_root;
        let workspace = root.to_string_lossy();
        let text = annotate_in_context(text, &workspace, directories, read_scope);
        let before = if root_changed {
            None
        } else {
            self.head_before.as_deref()
        };
        annotate_action_claims_at(
            text,
            collect_git_evidence(&workspace, read_scope, before).as_ref(),
            Some(&root),
            root_changed,
        )
    }
}
