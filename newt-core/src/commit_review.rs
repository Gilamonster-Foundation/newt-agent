//! #2905: commit-message review, independent of the shell route.
//!
//! Every commit a `run_command` invocation lands is read back from the object
//! store and reviewed here, whether it went through the native broker or ran
//! in the host shell under `--full-access`. Provenance is a deterministic
//! comparison against the attribution ledger's own rendering; this module
//! only reports — it never rewrites history.

use agent_toolchain::native_git::CommitPolicy;

/// Upper bound on commits reviewed after one invocation.
pub(crate) const MAX_REVIEWED_COMMITS: usize = 16;

/// What the ledger says a newt-attributed commit carries right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Expected {
    /// Non-blank lines of the canonical finalizer run over an empty message
    /// (the same rendering `/byline` shows). Empty when no policy exists.
    lines: Vec<String>,
    /// Address tags (`<…>`) that belong to newt's own machinery.
    newt_tags: Vec<String>,
    vouched: bool,
}

impl Expected {
    /// Capture the expectation BEFORE the command runs, from the same policy
    /// the native broker would finalize with.
    pub(crate) fn from_policy(policy: Option<&dyn CommitPolicy>) -> Self {
        let mut newt_tags = vec![format!("<{}>", crate::agent_identity::DEFAULT_AGENT_EMAIL)];
        let Some(policy) = policy else {
            return Self { lines: Vec::new(), newt_tags, vouched: false };
        };
        if let Some(email) = policy.agent_email() {
            let tag = format!("<{email}>");
            if !newt_tags.contains(&tag) {
                newt_tags.push(tag);
            }
        }
        match policy.finalize_message("") {
            Ok(rendered) => Self {
                lines: rendered
                    .lines()
                    .map(str::trim_end)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect(),
                newt_tags,
                vouched: true,
            },
            Err(_) => Self { lines: Vec::new(), newt_tags, vouched: false },
        }
    }

    fn claims_newt(&self, line: &str) -> bool {
        (line.starts_with("Co-authored-by: ")
            && self.newt_tags.iter().any(|tag| line.ends_with(tag.as_str())))
            || crate::attribution::is_newt_provenance(line)
    }
}

/// One review finding. Rendered into the tool result and shown to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finding {
    /// A newt-address trailer or provenance line the ledger does not render.
    Forged(String),
    /// A ledger line absent from a commit that otherwise claims newt provenance.
    Missing(String),
    /// A newt-address line with no ledger in this session to vouch for it.
    Unvouched(String),
}

/// Reads landed commits. Production is a fixed-argv hardened `git`; unit
/// tests use an in-memory fake. Never runs hooks or the model's shell text.
pub(crate) trait CommitReader {
    /// The current `HEAD` oid, or `None` (unborn branch / not a repository).
    fn head(&self) -> Option<String>;
    /// `(oid, full message)` for commits in `before..head`, newest first,
    /// at most `limit`. `before == None` means "only `head`".
    fn landed(&self, before: Option<&str>, head: &str, limit: usize) -> Vec<(String, String)>;
}

/// #2905 seam: review every commit an invocation landed. Called with the
/// `HEAD` read before the command and the expectation captured then. The
/// signature carries no route — confined and `--full-access` runs share it.
/// Returns the rendered block to append to the tool result, if any.
pub(crate) fn review_landed(
    reader: &dyn CommitReader,
    before: Option<&str>,
    expected: &Expected,
) -> Option<String> {
    let head = reader.head()?;
    if before == Some(head.as_str()) {
        return None;
    }
    let mut out = String::new();
    for (oid, message) in reader.landed(before, &head, MAX_REVIEWED_COMMITS) {
        let findings = check_trailers(&message, expected);
        if findings.is_empty() {
            continue;
        }
        let short = &oid[..oid.len().min(12)];
        out.push_str(&format!("commit review: {short} provenance does not match the attribution ledger\n"));
        for finding in findings {
            let (label, line) = match finding {
                Finding::Forged(l) => ("not rendered by the ledger", l),
                Finding::Missing(l) => ("missing from the commit", l),
                Finding::Unvouched(l) => ("no ledger in this session vouches for", l),
            };
            out.push_str(&format!("  - {label}: {line}\n"));
        }
    }
    if out.is_empty() {
        return None;
    }
    out.push_str(
        "  Attribution is harness-owned: `/byline` shows the exact block; amend the \
         message to match it or drop the newt-address lines.\n",
    );
    Some(out)
}

/// The review owed by one invocation: captured before the command runs,
/// finished after it. Built only from the command text and the session's
/// policy — no route, bypass, or broker parameter exists to depend on.
pub(crate) struct Pending {
    before: Option<String>,
    expected: Expected,
}

impl Pending {
    /// `commit_producing` is the existing lexical detector's verdict.
    pub(crate) fn start(
        commit_producing: bool,
        reader: &dyn CommitReader,
        policy: Option<&dyn CommitPolicy>,
    ) -> Option<Self> {
        commit_producing.then(|| Self {
            before: reader.head(),
            expected: Expected::from_policy(policy),
        })
    }

    pub(crate) fn finish(&self, reader: &dyn CommitReader) -> Option<String> {
        review_landed(reader, self.before.as_deref(), &self.expected)
    }
}

/// Production reader: fixed-argv hardened `git` (no hooks, no config, empty
/// env) in the command's directory. Read-only; failures read as "nothing".
pub(crate) struct HostRepo(pub(crate) std::path::PathBuf);

impl HostRepo {
    fn run(&self, args: &[&str]) -> Option<Vec<u8>> {
        let output = crate::git_hardening::hardened_git(&self.0, args)
            .ok()?
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }
}

impl CommitReader for HostRepo {
    fn head(&self) -> Option<String> {
        let out = self.run(&["rev-parse", "--verify", "-q", "HEAD"])?;
        let oid = String::from_utf8(out).ok()?.trim().to_owned();
        (!oid.is_empty()).then_some(oid)
    }

    fn landed(&self, before: Option<&str>, head: &str, limit: usize) -> Vec<(String, String)> {
        let max = format!("--max-count={}", if before.is_some() { limit } else { 1 });
        let range = before.map_or_else(|| head.to_owned(), |b| format!("{b}..{head}"));
        let Some(list) = self.run(&["rev-list", &max, &range]) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&list)
            .lines()
            .filter_map(|oid| {
                let raw = self.run(&["cat-file", "commit", oid])?;
                let at = raw.windows(2).position(|w| w == b"\n\n")?;
                Some((oid.to_owned(), String::from_utf8_lossy(&raw[at + 2..]).into_owned()))
            })
            .collect()
    }
}

/// Compare a landed commit message's newt-owned lines against the ledger.
pub(crate) fn check_trailers(message: &str, expected: &Expected) -> Vec<Finding> {
    let lines: Vec<&str> = message.lines().map(str::trim_end).collect();
    let claims: Vec<&str> = lines.iter().copied().filter(|l| expected.claims_newt(l)).collect();
    if !expected.vouched {
        return claims.into_iter().map(|l| Finding::Unvouched(l.into())).collect();
    }
    let mut findings: Vec<Finding> = claims
        .iter()
        .filter(|l| !expected.lines.iter().any(|e| e == *l))
        .map(|l| Finding::Forged((*l).into()))
        .collect();
    if !claims.is_empty() {
        // Model trailers are not required: a later commit in one chained
        // invocation legitimately carries only the unconsumed contributor
        // tail. Operator by-line and provenance are owed by every one.
        findings.extend(
            expected
                .lines
                .iter()
                .filter(|e| !expected.claims_newt(e) || crate::attribution::is_newt_provenance(e))
                .filter(|e| !lines.contains(&e.as_str()))
                .map(|e| Finding::Missing(e.clone())),
        );
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEWT: &str = crate::agent_identity::DEFAULT_AGENT_EMAIL;

    /// A fixed policy: the canonical finalizer of one `CommitAttribution`.
    struct LedgerPolicy(crate::attribution::CommitAttribution);
    impl CommitPolicy for LedgerPolicy {
        fn finalize_message(&self, message: &str) -> Result<String, String> {
            Ok(self.0.finalize_message(message))
        }
        fn agent_email(&self) -> Option<String> {
            Some(self.0.agent_email.clone())
        }
        fn signing_required(&self) -> bool {
            false
        }
        fn sign_commit(&self, _: &[u8]) -> Result<String, String> {
            Err("unused".into())
        }
        fn committed(&self) {}
    }

    fn ledger(agent_email: &str) -> crate::attribution::CommitAttribution {
        let mut a = crate::attribution::CommitAttribution::from_runtime(
            "claude-opus-5-5",
            Some("Op Erator".into()),
            agent_email,
        );
        a.harness_name = "newt-agent".into();
        a.harness_version = "0.8.0".into();
        a.harness_build_revision = "cad38c1394ef".into();
        a.operator_email = Some("op@example.com".into());
        a
    }

    fn expected(agent_email: &str) -> Expected {
        Expected::from_policy(Some(&LedgerPolicy(ledger(agent_email))))
    }

    /// The live #2905 bug, host-shell route: under `--full-access` the model
    /// typed its own trailer under newt's address with the worktree HEAD in
    /// place of the build, and no operator line. No broker ran.
    #[test]
    fn host_shell_route_wrong_build_trailer_is_flagged() {
        let message = format!(
            "fix: thing\n\nCo-authored-by: claude-opus-5-5 (v0.8.0 9ba3d8e3) <{NEWT}>\n"
        );
        let findings = check_trailers(&message, &expected(NEWT));
        assert!(
            findings.contains(&Finding::Forged(format!(
                "Co-authored-by: claude-opus-5-5 (v0.8.0 9ba3d8e3) <{NEWT}>"
            ))),
            "{findings:?}"
        );
        assert!(
            findings.contains(&Finding::Missing(
                "Co-authored-by: Op Erator <op@example.com>".into()
            )),
            "{findings:?}"
        );
    }

    /// Governed route: the broker finalized the message, but the identity
    /// overrides the agent email, so the finalizer's stale-trailer match
    /// (`attribution.rs` `is_newt_model_trailer`, agent email only) left a
    /// model-typed wrong-build line under the DEFAULT newt address in place.
    #[test]
    fn governed_route_wrong_build_trailer_is_flagged() {
        let custom = "bot@example.org";
        let typed = format!("Co-authored-by: claude-opus-5-5 (v0.8.0 9ba3d8e3) <{NEWT}>");
        let message = ledger(custom).finalize_message(&format!("fix: thing\n\n{typed}\n"));
        assert!(message.contains(&typed), "precondition: finalizer kept it");
        let findings = check_trailers(&message, &expected(custom));
        assert_eq!(findings, vec![Finding::Forged(typed)]);
    }

    #[test]
    fn canonical_governed_message_passes() {
        let message = ledger(NEWT).finalize_message("fix: thing\n\nbody\n");
        assert_eq!(check_trailers(&message, &expected(NEWT)), Vec::new());
    }

    #[test]
    fn unattributed_host_commit_is_not_a_provenance_finding() {
        let message = "fix: thing\n\nCo-authored-by: Someone <s@example.com>\n";
        assert_eq!(check_trailers(message, &expected(NEWT)), Vec::new());
    }

    #[test]
    fn forged_provenance_line_is_flagged() {
        let canonical = ledger(NEWT).finalize_message("fix: thing\n");
        let forged = canonical.replace("(cad38c1394ef)", "(9ba3d8e3)");
        let findings = check_trailers(&forged, &expected(NEWT));
        assert!(
            findings.iter().any(|f| matches!(f, Finding::Forged(l) if l.starts_with("Harness: "))),
            "{findings:?}"
        );
    }

    #[test]
    fn without_a_ledger_newt_lines_are_unvouched() {
        let line = format!("Co-authored-by: m (v0.8.0 abc) <{NEWT}>");
        let findings = check_trailers(&format!("x\n\n{line}\n"), &Expected::from_policy(None));
        assert_eq!(findings, vec![Finding::Unvouched(line)]);
    }

    /// In-memory history: newest-first `(oid, message)` pairs.
    struct FakeRepo(Vec<(String, String)>);
    impl CommitReader for FakeRepo {
        fn head(&self) -> Option<String> {
            self.0.first().map(|(oid, _)| oid.clone())
        }
        fn landed(&self, before: Option<&str>, _: &str, limit: usize) -> Vec<(String, String)> {
            self.0
                .iter()
                .take_while(|(oid, _)| Some(oid.as_str()) != before)
                .take(limit)
                .cloned()
                .collect()
        }
    }

    #[test]
    fn review_landed_reports_only_new_bad_commits() {
        let good = ledger(NEWT).finalize_message("good\n");
        let bad = format!("bad\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n");
        let repo = FakeRepo(vec![
            ("oid-three".into(), bad),
            ("oid-two".into(), good),
            ("oid-one".into(), "old\n\nCo-authored-by: x (v0) <".to_string() + NEWT + ">\n"),
        ]);
        let report = review_landed(&repo, Some("oid-one"), &expected(NEWT)).expect("flagged");
        assert!(report.contains("commit review: oid-three"), "{report}");
        assert!(!report.contains("oid-two") && !report.contains("oid-one"), "{report}");
        assert!(report.contains("9ba3d8e3") && report.contains("/byline"), "{report}");
    }

    #[test]
    fn review_landed_is_silent_when_head_did_not_move_or_is_canonical() {
        let repo = FakeRepo(vec![("c1".into(), ledger(NEWT).finalize_message("x\n"))]);
        assert_eq!(review_landed(&repo, Some("c1"), &expected(NEWT)), None);
        assert_eq!(review_landed(&repo, None, &expected(NEWT)), None);
        assert_eq!(review_landed(&FakeRepo(vec![]), None, &expected(NEWT)), None);
    }

    #[test]
    fn review_landed_is_bounded() {
        let bad = format!("b\n\nCo-authored-by: m (v0 x) <{NEWT}>\n");
        let repo = FakeRepo((0..40).map(|i| (format!("c{i:02}"), bad.clone())).collect());
        let report = review_landed(&repo, None, &expected(NEWT)).unwrap();
        assert_eq!(report.matches("commit review:").count(), MAX_REVIEWED_COMMITS);
    }

    /// The seam both routes pass through: `start` captures HEAD + the
    /// ledger before the command, `finish` reads what landed. The host-shell
    /// (`--full-access`) commit and the brokered one are the same call.
    #[test]
    fn pending_review_captures_before_and_flags_what_landed() {
        let policy = LedgerPolicy(ledger(NEWT));
        let before = FakeRepo(vec![("oid-base".into(), "base\n".into())]);
        let pending = Pending::start(true, &before, Some(&policy)).expect("commit-producing");
        let after = FakeRepo(vec![
            ("oid-new".into(), format!("x\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n")),
            ("oid-base".into(), "base\n".into()),
        ]);
        assert!(pending.finish(&after).unwrap().contains("oid-new"));
        assert!(Pending::start(false, &before, Some(&policy)).is_none());
    }

    /// Second commit of `git commit … && git commit …`: the first consumed
    /// the accumulated contributors, so only the active model renders. Not
    /// a provenance defect.
    #[test]
    fn chained_commit_without_consumed_contributors_passes() {
        let mut with_history = ledger(NEWT);
        with_history.contributors =
            vec![crate::attribution::Attribution::new("older", "newt-agent", "0.7.9", NEWT)];
        let second = ledger(NEWT).finalize_message("second\n");
        let expected = Expected::from_policy(Some(&LedgerPolicy(with_history)));
        assert_eq!(check_trailers(&second, &expected), Vec::new());
    }
}
