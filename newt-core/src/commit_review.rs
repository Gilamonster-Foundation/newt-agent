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
            return Self {
                lines: Vec::new(),
                newt_tags,
                vouched: false,
            };
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
            Err(_) => Self {
                lines: Vec::new(),
                newt_tags,
                vouched: false,
            },
        }
    }

    fn claims_newt(&self, line: &str) -> bool {
        (line.starts_with("Co-authored-by: ")
            && self
                .newt_tags
                .iter()
                .any(|tag| line.ends_with(tag.as_str())))
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
    /// `git show --stat` summary for one commit (empty when unreadable).
    fn stat(&self, oid: &str) -> String;
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
        let leaks = leak_findings(&message);
        let short = &oid[..oid.len().min(12)];
        if !leaks.is_empty() {
            out.push_str(&format!("commit review: {short} message content\n"));
            for leak in leaks {
                out.push_str(&format!("  - {leak}\n"));
            }
        }
        if findings.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "commit review: {short} provenance does not match the attribution ledger\n"
        ));
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
    if out.contains("provenance does not match") {
        out.push_str(
            "  Attribution is harness-owned: `/byline` shows the exact block; amend the \
             message to match it or drop the newt-address lines.\n",
        );
    }
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

    /// `(oid, prompt)` for the model content review of what landed, newest
    /// first, at most [`MAX_CONTENT_REVIEWS`]. Empty when HEAD did not move.
    pub(crate) fn content_prompts(&self, reader: &dyn CommitReader) -> Vec<(String, String)> {
        let Some(head) = reader.head() else {
            return Vec::new();
        };
        if self.before.as_deref() == Some(head.as_str()) {
            return Vec::new();
        }
        reader
            .landed(self.before.as_deref(), &head, MAX_CONTENT_REVIEWS)
            .into_iter()
            .take(MAX_CONTENT_REVIEWS)
            .map(|(oid, message)| {
                let prompt = content_prompt(&message, &reader.stat(&oid));
                (oid, prompt)
            })
            .collect()
    }
}

/// Render one commit's model findings, flag-only. `None` when clean.
pub(crate) fn render_content(oid: &str, findings: &[String]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let short = &oid[..oid.len().min(12)];
    let mut out = format!("commit review: {short} content (model, advisory)\n");
    for finding in findings {
        out.push_str(&format!("  - {finding}\n"));
    }
    Some(out)
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
                Some((
                    oid.to_owned(),
                    String::from_utf8_lossy(&raw[at + 2..]).into_owned(),
                ))
            })
            .collect()
    }

    fn stat(&self, oid: &str) -> String {
        self.run(&["show", "--stat", "--format=", "--no-color", oid])
            .map(|out| String::from_utf8_lossy(&out).into_owned())
            .unwrap_or_default()
    }
}

/// Leaks a commit message must never carry (CLAUDE.md "What must never leave
/// the machine"; `.githooks/commit-msg`). Data, not logic: `(needle, label)`,
/// matched case-insensitively. A path needle must be followed by a name.
const LEAK_PATTERNS: &[(&str, &str)] = &[
    ("/home/", "absolute local path"),
    ("/users/", "absolute local path"),
    ("claude.ai/code/session", "harness session link"),
    ("claude-session:", "harness session link"),
];

/// Deterministic leak scan. Runs with or without an auxiliary model.
pub(crate) fn leak_findings(message: &str) -> Vec<String> {
    let lower = message.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    for (needle, label) in LEAK_PATTERNS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(needle).map(|i| i + from) {
            from = at + needle.len();
            let token_end = message[at..]
                .find(char::is_whitespace)
                .map_or(message.len(), |i| at + i);
            let token = &message[at..token_end];
            // A path needle must name someone (`/home/<name>`), not stand alone.
            if needle.ends_with('/') && token.len() <= needle.len() {
                continue;
            }
            let finding = format!("leak ({label}): {token}");
            if !out.contains(&finding) {
                out.push(finding);
            }
        }
    }
    out
}

/// Commits per invocation that get a model content review.
pub(crate) const MAX_CONTENT_REVIEWS: usize = 4;
const CONTENT_INPUT_CAP: usize = 8 * 1024;
const CONTENT_KINDS: &[&str] = &["overclaim", "inaccurate", "leak"];

fn capped(text: &str) -> &str {
    let mut end = text.len().min(CONTENT_INPUT_CAP);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The bounded, tool-less content-review prompt: message + diff stat only.
pub(crate) fn content_prompt(message: &str, stat: &str) -> String {
    let evidence =
        serde_json::json!({"commit_message": capped(message), "diff_stat": capped(stat)});
    format!(
        "Review this commit message against its diff stat. Flag only: claims the \
         stat cannot support (overclaim), statements contradicted by the stat \
         (inaccurate), and private data such as local paths, hostnames or \
         session links (leak). Attribution trailers are checked elsewhere; ignore \
         them. Reply with JSON only: {{\"findings\":[{{\"kind\":\"overclaim|inaccurate|leak\",\
         \"detail\":\"...\"}}]}} — an empty list when nothing is wrong.\n{evidence}"
    )
}

/// Parse the reviewer's JSON. Malformed output yields nothing — a finding is
/// never fabricated from an unparseable reply.
pub(crate) fn parse_content_findings(raw: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Item {
        kind: String,
        detail: String,
    }
    #[derive(serde::Deserialize)]
    struct Reply {
        findings: Vec<Item>,
    }
    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
        return Vec::new();
    };
    let Ok(reply) = serde_json::from_str::<Reply>(raw.get(start..=end).unwrap_or_default()) else {
        return Vec::new();
    };
    reply
        .findings
        .into_iter()
        .filter(|item| {
            CONTENT_KINDS.contains(&item.kind.as_str()) && !item.detail.trim().is_empty()
        })
        .map(|item| format!("{}: {}", item.kind, item.detail.trim()))
        .collect()
}

/// Compare a landed commit message's newt-owned lines against the ledger.
pub(crate) fn check_trailers(message: &str, expected: &Expected) -> Vec<Finding> {
    let lines: Vec<&str> = message.lines().map(str::trim_end).collect();
    let claims: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| expected.claims_newt(l))
        .collect();
    if !expected.vouched {
        return claims
            .into_iter()
            .map(|l| Finding::Unvouched(l.into()))
            .collect();
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
        let message =
            format!("fix: thing\n\nCo-authored-by: claude-opus-5-5 (v0.8.0 9ba3d8e3) <{NEWT}>\n");
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
            findings
                .iter()
                .any(|f| matches!(f, Finding::Forged(l) if l.starts_with("Harness: "))),
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
        fn stat(&self, oid: &str) -> String {
            format!(" {oid}.rs | 1 +")
        }
    }

    #[test]
    fn review_landed_reports_only_new_bad_commits() {
        let good = ledger(NEWT).finalize_message("good\n");
        let bad = format!("bad\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n");
        let repo = FakeRepo(vec![
            ("oid-three".into(), bad),
            ("oid-two".into(), good),
            (
                "oid-one".into(),
                "old\n\nCo-authored-by: x (v0) <".to_string() + NEWT + ">\n",
            ),
        ]);
        let report = review_landed(&repo, Some("oid-one"), &expected(NEWT)).expect("flagged");
        assert!(report.contains("commit review: oid-three"), "{report}");
        assert!(
            !report.contains("oid-two") && !report.contains("oid-one"),
            "{report}"
        );
        assert!(
            report.contains("9ba3d8e3") && report.contains("/byline"),
            "{report}"
        );
    }

    #[test]
    fn review_landed_is_silent_when_head_did_not_move_or_is_canonical() {
        let repo = FakeRepo(vec![("c1".into(), ledger(NEWT).finalize_message("x\n"))]);
        assert_eq!(review_landed(&repo, Some("c1"), &expected(NEWT)), None);
        assert_eq!(review_landed(&repo, None, &expected(NEWT)), None);
        assert_eq!(
            review_landed(&FakeRepo(vec![]), None, &expected(NEWT)),
            None
        );
    }

    #[test]
    fn review_landed_is_bounded() {
        let bad = format!("b\n\nCo-authored-by: m (v0 x) <{NEWT}>\n");
        let repo = FakeRepo((0..40).map(|i| (format!("c{i:02}"), bad.clone())).collect());
        let report = review_landed(&repo, None, &expected(NEWT)).unwrap();
        assert_eq!(
            report.matches("commit review:").count(),
            MAX_REVIEWED_COMMITS
        );
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
            (
                "oid-new".into(),
                format!("x\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n"),
            ),
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
        with_history.contributors = vec![crate::attribution::Attribution::new(
            "older",
            "newt-agent",
            "0.7.9",
            NEWT,
        )];
        let second = ledger(NEWT).finalize_message("second\n");
        let expected = Expected::from_policy(Some(&LedgerPolicy(with_history)));
        assert_eq!(check_trailers(&second, &expected), Vec::new());
    }

    #[test]
    fn leak_scan_flags_local_paths_and_session_links() {
        let msg = "fix\n\nsee /home/alice/src/x.rs and https://claude.ai/code/session_abc\n\
                   Claude-Session: https://claude.ai/code/session_abc\n";
        let found = leak_findings(msg);
        assert!(found.iter().any(|f| f.contains("/home/alice")), "{found:?}");
        assert!(
            found.iter().any(|f| f.contains("session link")),
            "{found:?}"
        );
        assert!(leak_findings("fix /Users/bob/x")
            .iter()
            .any(|f| f.contains("/Users/bob")));
    }

    #[test]
    fn leak_scan_passes_repo_relative_text() {
        assert_eq!(
            leak_findings("fix newt-core/src/x.rs; /home/ alone; Co-authored-by: a <b>"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn content_prompt_carries_message_and_stat_and_is_capped() {
        let p = content_prompt("fix the parser", " src/a.rs | 2 +-");
        assert!(
            p.contains("fix the parser") && p.contains("src/a.rs | 2"),
            "{p}"
        );
        assert!(p.contains("\"findings\""), "asks for the JSON shape: {p}");
        let huge = "x".repeat(100_000);
        assert!(content_prompt(&huge, &huge).len() < 3 * CONTENT_INPUT_CAP);
    }

    #[test]
    fn parse_content_findings_accepts_json_and_drops_garbage() {
        let raw = r#"noise {"findings":[{"kind":"overclaim","detail":"says tests added; stat shows none"}]} tail"#;
        assert_eq!(
            parse_content_findings(raw),
            vec!["overclaim: says tests added; stat shows none".to_string()]
        );
        assert_eq!(
            parse_content_findings(r#"{"findings":[]}"#),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_content_findings("looks fine to me"),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_content_findings(r#"{"findings":[{"kind":"bogus","detail":"x"}]}"#),
            Vec::<String>::new()
        );
    }

    #[test]
    fn review_landed_reports_leaks_without_a_byline_hint() {
        let repo = FakeRepo(vec![("oid-new".into(), "fix /home/alice/x\n".into())]);
        let report = review_landed(&repo, None, &expected(NEWT)).expect("leak flagged");
        assert!(
            report.contains("oid-new message content") && report.contains("/home/alice/x"),
            "{report}"
        );
        assert!(!report.contains("/byline"), "{report}");
    }

    #[test]
    fn content_prompts_cover_landed_commits_bounded() {
        let before = FakeRepo(vec![("oid-base".into(), "b\n".into())]);
        let pending = Pending::start(true, &before, None).unwrap();
        let mut history: Vec<(String, String)> = (0..10)
            .map(|i| (format!("oid-{i}"), format!("msg {i}\n")))
            .collect();
        history.push(("oid-base".into(), "b\n".into()));
        let prompts = pending.content_prompts(&FakeRepo(history));
        assert_eq!(prompts.len(), MAX_CONTENT_REVIEWS);
        assert_eq!(prompts[0].0, "oid-0");
        assert!(
            prompts[0].1.contains("msg 0") && prompts[0].1.contains("oid-0.rs | 1 +"),
            "{}",
            prompts[0].1
        );
        assert!(
            pending.content_prompts(&before).is_empty(),
            "HEAD did not move"
        );
    }

    #[test]
    fn render_content_is_flag_only_and_silent_when_clean() {
        assert_eq!(render_content("oid-x", &[]), None);
        let r = render_content("oid-x", &["overclaim: no tests in stat".into()]).unwrap();
        assert!(
            r.contains("commit review: oid-x content (model, advisory)") && r.contains("overclaim"),
            "{r}"
        );
    }
}
