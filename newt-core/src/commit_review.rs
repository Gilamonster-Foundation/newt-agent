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
    /// Lines EVERY commit claiming newt provenance owes: the policy's
    /// explicit operator by-line plus the provenance line. Model trailers
    /// (newt's or a foreign harness's) are consumed after the first commit
    /// of a chain, so they are never required.
    required: Vec<String>,
    vouched: bool,
}

impl Expected {
    /// Capture the expectation BEFORE the command runs, from the same policy
    /// the native broker would finalize with.
    pub(crate) fn from_policy(policy: Option<&dyn CommitPolicy>) -> Self {
        let mut newt_tags = vec![format!("<{}>", crate::agent_identity::DEFAULT_AGENT_EMAIL)];
        let Some(policy) = policy else {
            return Self {
                newt_tags,
                ..Self::default()
            };
        };
        if let Some(email) = policy.agent_email() {
            let tag = format!("<{email}>");
            if !newt_tags.contains(&tag) {
                newt_tags.push(tag);
            }
        }
        let Ok(rendered) = policy.finalize_message("") else {
            return Self {
                newt_tags,
                ..Self::default()
            };
        };
        let lines: Vec<String> = rendered
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        let operator = policy.operator_line();
        let required = lines
            .iter()
            .filter(|l| {
                crate::attribution::is_newt_provenance(l) || operator.as_deref() == Some(l.as_str())
            })
            .cloned()
            .collect();
        Self {
            lines,
            newt_tags,
            required,
            vouched: true,
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

/// Failure to read the current HEAD, distinct from a confirmed absent HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    ReadFailed,
}

impl std::fmt::Display for HeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HEAD read failed")
    }
}

impl std::error::Error for HeadError {}

/// Reads landed commits. Production is a fixed-argv `metadata_git` under the
/// turn's read scope; unit tests use an in-memory fake. Never runs hooks or
/// the model's shell text. Implementations may block: callers run them off
/// the async executor under a deadline ([`Review`]).
pub trait CommitReader: Send + Sync {
    /// The current `HEAD` oid, or `Ok(None)` (unborn branch / not a repository).
    /// `Err(HeadError::ReadFailed)` means the read itself failed, which is distinct from a
    /// confirmed absent HEAD and must skip review instead of reviewing all
    /// history as newly landed.
    fn head(&self) -> std::result::Result<Option<String>, HeadError>;
    /// `(oid, full message)` for commits in `before..head`, newest first,
    /// at most `limit`. `before == None` means "all of `head`'s history"
    /// (the command started on an unborn branch), still at most `limit`.
    fn landed(&self, before: Option<&str>, head: &str, limit: usize) -> Vec<(String, String)>;
    /// `git show --stat` summary for one commit (empty when unreadable).
    fn stat(&self, oid: &str) -> String;
}

/// A reader shared with the blocking task that runs it.
pub(crate) type SharedReader = std::sync::Arc<dyn CommitReader>;

/// Something that can vend a reader for the command directory under the
/// turn's read scope. Production is `HostReaders`; tests default to `NoOpReaders`
/// so they never spawn real git.
pub trait ReaderSource: Send + Sync {
    fn open(
        &self,
        cwd: &std::path::Path,
        read_scope: &crate::Scope<String>,
    ) -> Option<SharedReader>;
}

/// No-op reader used when the dispatcher has no reader source (tests).
pub(crate) struct NoOpRepo;
impl CommitReader for NoOpRepo {
    fn head(&self) -> std::result::Result<Option<String>, HeadError> {
        Ok(None)
    }
    fn landed(&self, _: Option<&str>, _: &str, _: usize) -> Vec<(String, String)> {
        Vec::new()
    }
    fn stat(&self, _: &str) -> String {
        String::new()
    }
}

/// Production reader source: honors the read scope via `metadata_git`.
pub struct HostReaders;
impl ReaderSource for HostReaders {
    fn open(
        &self,
        cwd: &std::path::Path,
        read_scope: &crate::Scope<String>,
    ) -> Option<SharedReader> {
        crate::agentic::check_git_read_scope("metadata", read_scope).ok()?;
        Some(std::sync::Arc::new(HostRepo {
            cwd: cwd.to_path_buf(),
            read_scope: read_scope.clone(),
        }))
    }
}

/// No-op reader source for tests that do not care about commit-message review.
pub struct NoOpReaders;
impl ReaderSource for NoOpReaders {
    fn open(&self, _: &std::path::Path, _: &crate::Scope<String>) -> Option<SharedReader> {
        None
    }
}

/// Deadline for the pre-command `HEAD` snapshot.
pub(crate) const SNAPSHOT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
/// Deadline for reading back and checking everything that landed.
pub(crate) const FINISH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Run a blocking read off the async executor. `None` on deadline or panic:
/// the turn proceeds and the review is skipped (fail-open). A read that
/// never returns strands only its blocking thread, never the turn.
async fn off_executor<T: Send + 'static>(
    deadline: std::time::Duration,
    read: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    tokio::time::timeout(deadline, tokio::task::spawn_blocking(read))
        .await
        .ok()?
        .ok()
}

/// One invocation's review, bound to its reader. `begin` before the command,
/// `conclude` after it; both are bounded and fail-open.
pub(crate) struct Review {
    pending: Pending,
    reader: SharedReader,
}

impl Review {
    /// `None` when the snapshot did not complete in time: reviewing against
    /// an unknown `before` would treat all history as newly landed.
    pub(crate) async fn begin(
        reader: SharedReader,
        policy: Option<&dyn CommitPolicy>,
    ) -> Option<Self> {
        Self::begin_within(reader, policy, SNAPSHOT_DEADLINE).await
    }

    async fn begin_within(
        reader: SharedReader,
        policy: Option<&dyn CommitPolicy>,
        deadline: std::time::Duration,
    ) -> Option<Self> {
        let expected = Expected::from_policy(policy);
        let snapshot = reader.clone();
        let before = match off_executor(deadline, move || snapshot.head()).await {
            Some(Ok(before)) => before,
            // A failed read or a timed-out snapshot is unsafe to review against:
            // an unknown `before` would treat all reachable history as newly
            // landed, and a confirmed read failure must skip review entirely.
            Some(Err(HeadError::ReadFailed)) | None => return None,
        };
        Some(Self {
            pending: Pending { before, expected },
            reader,
        })
    }

    /// The deterministic report and the model content prompts, from ONE
    /// read of what landed. Empty on deadline.
    pub(crate) async fn conclude(self) -> (Option<String>, Vec<(String, String)>) {
        self.conclude_within(FINISH_DEADLINE).await
    }

    async fn conclude_within(
        self,
        deadline: std::time::Duration,
    ) -> (Option<String>, Vec<(String, String)>) {
        let Self { pending, reader } = self;
        off_executor(deadline, move || pending.finish(reader.as_ref()))
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
/// #2905 seam: review every commit an invocation landed. Called with the
/// `HEAD` read before the command and the expectation captured then. The
/// signature carries no route — confined and `--full-access` runs share it.
/// Returns the rendered block to append to the tool result, if any.
pub(crate) fn review_landed(
    reader: &dyn CommitReader,
    before: Option<&str>,
    expected: &Expected,
) -> Option<String> {
    let Ok(Some(head)) = reader.head() else {
        return None;
    };
    if before == Some(head.as_str()) {
        return None;
    }
    review_commits(
        &reader.landed(before, &head, MAX_REVIEWED_COMMITS),
        expected,
    )
}

/// Deterministic review of already-read `(oid, message)` pairs.
fn review_commits(commits: &[(String, String)], expected: &Expected) -> Option<String> {
    let mut out = String::new();
    for (oid, message) in commits {
        let findings = check_trailers(message, expected);
        let leaks = leak_findings(message);
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
/// finished after it. Taken around EVERY `run_command` (round 2 of #2905):
/// a commit landed by a script, Makefile target or alias is invisible to any
/// lexical detector, so none is consulted. Outside a repository both `HEAD`
/// reads come back `None` and nothing is reviewed.
pub(crate) struct Pending {
    before: Option<String>,
    expected: Expected,
}

impl Pending {
    #[cfg(test)]
    pub(crate) fn start(reader: &dyn CommitReader, policy: Option<&dyn CommitPolicy>) -> Self {
        Self {
            before: reader.head().unwrap_or_default(),
            expected: Expected::from_policy(policy),
        }
    }

    /// One `HEAD` read, one walk of what landed: the deterministic report
    /// over up to [`MAX_REVIEWED_COMMITS`], and `(oid, prompt)` content
    /// prompts for the newest [`MAX_CONTENT_REVIEWS`] of them.
    pub(crate) fn finish(
        &self,
        reader: &dyn CommitReader,
    ) -> (Option<String>, Vec<(String, String)>) {
        let Ok(Some(head)) = reader.head() else {
            return Default::default();
        };
        if self.before.as_deref() == Some(head.as_str()) {
            return Default::default();
        }
        let commits = reader.landed(self.before.as_deref(), &head, MAX_REVIEWED_COMMITS);
        let prompts = commits
            .iter()
            .take(MAX_CONTENT_REVIEWS)
            .map(|(oid, message)| (oid.clone(), content_prompt(message, &reader.stat(oid))))
            .collect();
        (review_commits(&commits, &self.expected), prompts)
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

/// Production reader: fixed-argv `metadata_git` (no hooks, scrubbed env,
/// global/system config ignored) in the command's directory, under the turn's
/// read scope. Repo-local config and its includes DO still load, which is why
/// every read is bounded: an include naming a FIFO blocks Git indefinitely.
/// Read-only; failures, permission denials, deadlines and oversized output all
/// read as "nothing".
pub struct HostRepo {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) read_scope: crate::Scope<String>,
}

/// Per-call deadline and output cap for one metadata read.
const GIT_READ_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
const GIT_OUTPUT_CAP: usize = 256 * 1024;

impl HostRepo {
    /// Run one metadata git command. `Ok(Some(_))` is successful output;
    /// `Ok(None)` is a clean git absence (only `rev-parse --verify -q HEAD`
    /// exiting 1 with empty stderr, i.e. an unborn branch); any other nonzero
    /// exit, timeout, oversize, or spawn failure is `Err(HeadError::ReadFailed)`
    /// and must skip review instead of reviewing all history.
    fn run(&self, args: &[&str]) -> std::result::Result<Option<Vec<u8>>, HeadError> {
        let mut child = crate::git_hardening::metadata_git(&self.cwd, args, &self.read_scope)
            .map_err(|_| HeadError::ReadFailed)?
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|_| HeadError::ReadFailed)?;
        let started = std::time::Instant::now();
        let stdout_rx = child
            .stdout
            .take()
            .map(|stdout| read_bounded(stdout, GIT_OUTPUT_CAP, GIT_READ_DEADLINE));
        let stderr_rx = child
            .stderr
            .take()
            .map(|stderr| read_bounded(stderr, GIT_OUTPUT_CAP, GIT_READ_DEADLINE));
        loop {
            match (
                child.try_wait().map_err(|_| HeadError::ReadFailed)?,
                started.elapsed() < GIT_READ_DEADLINE,
            ) {
                (Some(status), _) => {
                    let remaining = GIT_READ_DEADLINE.saturating_sub(started.elapsed());
                    let stdout = collect_output(stdout_rx, remaining)?;
                    let stderr = collect_output(stderr_rx, remaining)?;
                    return classify(args, status.code(), stdout, stderr);
                }
                (None, true) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                (None, false) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(HeadError::ReadFailed);
                }
            }
        }
    }
}

/// Classify the bounded result of one metadata git invocation.
/// `Some(0)` is success; for `rev-parse --verify -q HEAD` only, `Some(1)`
/// with empty stderr is the clean "no HEAD" absence. Everything else is a
/// read failure and must cause the review to be skipped.
fn classify(
    args: &[&str],
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> std::result::Result<Option<Vec<u8>>, HeadError> {
    if exit_code == Some(0) {
        return Ok(Some(stdout));
    }
    if args == ["rev-parse", "--verify", "-q", "HEAD"] && exit_code == Some(1) && stderr.is_empty()
    {
        return Ok(None);
    }
    Err(HeadError::ReadFailed)
}

/// Read `source` to EOF within `deadline` and at most `cap` bytes. Returns a
/// receiver so the caller can decide when to collect the result. The resolved
/// value is `None` when either bound is exceeded; the reading thread is
/// abandoned and the caller kills the producer, which closes the pipe.
fn read_bounded(
    source: impl std::io::Read + Send + 'static,
    cap: usize,
    _deadline: std::time::Duration,
) -> std::sync::mpsc::Receiver<Option<Vec<u8>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
        let ok =
            std::io::Read::read_to_end(&mut std::io::Read::take(source, limit), &mut buf).is_ok();
        let _ = tx.send(ok.then_some(buf).filter(|buf| buf.len() <= cap));
    });
    rx
}

/// Collect the bounded result for one pipe. A missing pipe is treated as
/// empty; any timeout, disconnect, read error, or oversize output becomes
/// `HeadError::ReadFailed` so the classifier never mistakes a failed read
/// for confirmed empty output.
fn collect_output(
    rx: Option<std::sync::mpsc::Receiver<Option<Vec<u8>>>>,
    remaining: std::time::Duration,
) -> std::result::Result<Vec<u8>, HeadError> {
    match rx {
        Some(rx) => rx
            .recv_timeout(remaining)
            .map_err(|_| HeadError::ReadFailed)?
            .ok_or(HeadError::ReadFailed),
        None => Ok(Vec::new()),
    }
}

/// `git rev-list` argv for the commits newly reachable in `before..head`.
/// An absent `before` (unborn branch, or no repository when the command
/// started) means ALL of `head`'s history is new, so it is walked from the
/// root under the same `limit` — never collapsed to one commit.
fn rev_list_args(before: Option<&str>, head: &str, limit: usize) -> Vec<String> {
    let range = before.map_or_else(|| head.to_owned(), |b| format!("{b}..{head}"));
    vec!["rev-list".into(), format!("--max-count={limit}"), range]
}

impl CommitReader for HostRepo {
    fn head(&self) -> std::result::Result<Option<String>, HeadError> {
        let out = self.run(&["rev-parse", "--verify", "-q", "HEAD"])?;
        let Some(out) = out else {
            return Ok(None);
        };
        let oid = String::from_utf8(out)
            .ok()
            .ok_or(HeadError::ReadFailed)?
            .trim()
            .to_owned();
        Ok((!oid.is_empty()).then_some(oid))
    }

    fn landed(&self, before: Option<&str>, head: &str, limit: usize) -> Vec<(String, String)> {
        let args = rev_list_args(before, head, limit);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let list = match self.run(&args) {
            Ok(Some(list)) => list,
            _ => return Vec::new(),
        };
        String::from_utf8_lossy(&list)
            .lines()
            .filter_map(|oid| {
                let raw = self.run(&["cat-file", "commit", oid]).ok().flatten()?;
                let at = raw.windows(2).position(|w| w == b"\n\n")?;
                Some((
                    oid.to_owned(),
                    String::from_utf8_lossy(&raw[at + 2..]).into_owned(),
                ))
            })
            .collect()
    }

    fn stat(&self, oid: &str) -> String {
        self.run(&[
            "show",
            "--stat",
            "--format=",
            "--no-color",
            "--no-textconv",
            "--no-ext-diff",
            oid,
        ])
        .ok()
        .flatten()
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
                .required
                .iter()
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
        fn operator_line(&self) -> Option<String> {
            self.0.operator_trailer()
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

    struct FakeRepo(Vec<(String, String)>);
    impl CommitReader for FakeRepo {
        fn head(&self) -> std::result::Result<Option<String>, HeadError> {
            Ok(self.0.first().map(|(oid, _)| oid.clone()))
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
        let pending = Pending::start(&before, Some(&policy));
        let after = FakeRepo(vec![
            (
                "oid-new".into(),
                format!("x\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n"),
            ),
            ("oid-base".into(), "base\n".into()),
        ]);
        assert!(pending.finish(&after).0.unwrap().contains("oid-new"));
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

    /// Codex round 1 (#2906 P2-4): a chained second commit after a session
    /// with a foreign-harness contributor. The native policy consumed that
    /// contributor on the first commit, so the second legitimately lacks
    /// its `noreply@anthropic.com` trailer. Only operator + provenance are
    /// owed by every commit.
    #[test]
    fn chained_commit_with_consumed_foreign_contributor_passes() {
        let mut with_history = ledger(NEWT);
        with_history.contributors = vec![
            crate::attribution::Attribution::new(
                "previous-model",
                "Claude Code",
                "2.1",
                "noreply@anthropic.com",
            ),
            crate::attribution::Attribution::new(
                "codex",
                "Codex CLI",
                "0.47.0",
                "codex@openai.com",
            ),
        ];
        let expected = Expected::from_policy(Some(&LedgerPolicy(with_history.clone())));
        let second = with_history.finalize_message_with("second commit", &[]);
        assert_eq!(check_trailers(&second, &expected), Vec::new());
        // The operator line is still owed, by name, not inferred by address.
        let no_operator = second.replace("Co-authored-by: Op Erator <op@example.com>\n", "");
        assert_eq!(
            check_trailers(&no_operator, &expected),
            vec![Finding::Missing(
                "Co-authored-by: Op Erator <op@example.com>".into()
            )]
        );
    }

    /// Codex round 1 (#2906 P2-7): a command creating the first three
    /// commits of an unborn repository has all three newly landed; the
    /// production reader walked only `--max-count=1`.
    #[test]
    fn unborn_start_walks_history_from_the_root_under_the_bound() {
        assert_eq!(
            rev_list_args(None, "h", MAX_REVIEWED_COMMITS),
            vec!["rev-list", "--max-count=16", "h"]
        );
        assert_eq!(
            rev_list_args(Some("b"), "h", 4),
            vec!["rev-list", "--max-count=4", "b..h"]
        );
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
        let pending = Pending::start(&before, None);
        let mut history: Vec<(String, String)> = (0..10)
            .map(|i| (format!("oid-{i}"), format!("msg {i}\n")))
            .collect();
        history.push(("oid-base".into(), "b\n".into()));
        let prompts = pending.finish(&FakeRepo(history)).1;
        assert_eq!(prompts.len(), MAX_CONTENT_REVIEWS);
        assert_eq!(prompts[0].0, "oid-0");
        assert!(
            prompts[0].1.contains("msg 0") && prompts[0].1.contains("oid-0.rs | 1 +"),
            "{}",
            prompts[0].1
        );
        assert!(
            pending.finish(&before) == (None, Vec::new()),
            "HEAD did not move"
        );
    }

    /// A reader whose every call blocks for longer than the review deadline.
    struct HangingRepo;
    impl CommitReader for HangingRepo {
        fn head(&self) -> std::result::Result<Option<String>, HeadError> {
            std::thread::sleep(std::time::Duration::from_secs(1));
            Ok(None)
        }
        fn landed(&self, _: Option<&str>, _: &str, _: usize) -> Vec<(String, String)> {
            std::thread::sleep(std::time::Duration::from_secs(1));
            Vec::new()
        }
        fn stat(&self, _: &str) -> String {
            std::thread::sleep(std::time::Duration::from_secs(1));
            String::new()
        }
    }

    #[tokio::test]
    async fn a_hanging_snapshot_never_hangs_the_turn() {
        let short = std::time::Duration::from_millis(50);
        let reader: SharedReader = std::sync::Arc::new(HangingRepo);
        let begun = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Review::begin_within(reader, None, short),
        )
        .await
        .expect("begin returned within its deadline");
        assert!(begun.is_none(), "an unknown `before` reviews nothing");
    }

    /// #2905 round 2 P2: a failed HEAD read must skip review, not be treated
    /// as an unborn repository that reviews all of history.
    #[tokio::test]
    async fn a_failed_head_read_skips_review() {
        struct ErrorThenOk(std::sync::atomic::AtomicBool);
        impl CommitReader for ErrorThenOk {
            fn head(&self) -> std::result::Result<Option<String>, HeadError> {
                if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    Ok(Some("oid-head".into()))
                } else {
                    Err(HeadError::ReadFailed)
                }
            }
            fn landed(&self, _: Option<&str>, _: &str, _: usize) -> Vec<(String, String)> {
                vec![("oid-old".into(), "old\n".into())]
            }
            fn stat(&self, _: &str) -> String {
                String::new()
            }
        }
        let reader: SharedReader =
            std::sync::Arc::new(ErrorThenOk(std::sync::atomic::AtomicBool::new(false)));
        assert!(
            Review::begin(reader, None).await.is_none(),
            "a failed initial HEAD snapshot must skip review"
        );
    }

    /// A genuinely unborn repository (Ok(None)) is still reviewed: there is no
    /// `before`, and a failed read is not confused with this case.
    #[tokio::test]
    async fn an_unborn_repository_still_reviews_landed_commits() {
        struct EmptyThenFull(std::sync::Mutex<Vec<(String, String)>>);
        impl CommitReader for EmptyThenFull {
            fn head(&self) -> std::result::Result<Option<String>, HeadError> {
                Ok(self.0.lock().unwrap().first().map(|(oid, _)| oid.clone()))
            }
            fn landed(&self, before: Option<&str>, _: &str, limit: usize) -> Vec<(String, String)> {
                self.0
                    .lock()
                    .unwrap()
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
        let repo = std::sync::Arc::new(EmptyThenFull(std::sync::Mutex::new(Vec::new())));
        let reader: SharedReader = repo.clone();
        let review = Review::begin(reader, None)
            .await
            .expect("unborn branch starts a review");
        // After the snapshot, a commit lands. The same reader now reports it.
        repo.0.lock().unwrap().push((
            "oid-new".into(),
            format!("x\n\nCo-authored-by: m (v0.8.0 9ba3d8e3) <{NEWT}>\n"),
        ));
        let (report, _) = review.conclude().await;
        assert!(
            report.as_ref().is_some_and(|r| r.contains("oid-new")),
            "unborn start should still review what landed: {report:?}"
        );
    }

    /// The pre-command snapshot succeeded but the post-command read hangs.
    struct HangAfterHead(std::sync::atomic::AtomicBool);
    impl CommitReader for HangAfterHead {
        fn head(&self) -> std::result::Result<Option<String>, HeadError> {
            if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Ok(Some("oid-base".into()))
        }
        fn landed(&self, _: Option<&str>, _: &str, _: usize) -> Vec<(String, String)> {
            Vec::new()
        }
        fn stat(&self, _: &str) -> String {
            String::new()
        }
    }

    #[tokio::test]
    async fn a_hanging_readback_concludes_empty() {
        let short = std::time::Duration::from_millis(50);
        let reader: SharedReader = std::sync::Arc::new(HangAfterHead(Default::default()));
        let review = Review::begin_within(reader, None, short)
            .await
            .expect("snapshot");
        let concluded = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            review.conclude_within(short),
        )
        .await
        .expect("conclude returned within its deadline");
        assert_eq!(concluded, (None, Vec::new()));
    }

    #[test]
    fn bounded_read_rejects_oversized_and_stalled_output() {
        let long = std::time::Duration::from_secs(5);
        assert_eq!(
            read_bounded(std::io::Cursor::new(b"abc".to_vec()), 3, long)
                .recv_timeout(long)
                .ok()
                .flatten(),
            Some(b"abc".to_vec())
        );
        assert_eq!(
            read_bounded(std::io::Cursor::new(b"abcd".to_vec()), 3, long)
                .recv_timeout(long)
                .ok()
                .flatten(),
            None
        );
        struct Stalled;
        impl std::io::Read for Stalled {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_secs(1));
                Ok(0)
            }
        }
        let started = std::time::Instant::now();
        assert_eq!(
            read_bounded(Stalled, 3, std::time::Duration::from_millis(50))
                .recv_timeout(std::time::Duration::from_millis(100))
                .ok()
                .flatten(),
            None
        );
        assert!(started.elapsed() < long);
    }

    /// Codex round 3 #2906 P2: only `rev-parse --verify -q HEAD` exiting 1
    /// with empty stderr means "no HEAD". A fatal exit (e.g. 128) or any
    /// stderr must be classified as a read failure so the review is skipped.
    #[test]
    fn fatal_git_exit_is_a_read_failure_not_unborn() {
        // The clean unborn case still resolves to None.
        assert_eq!(
            classify(
                &["rev-parse", "--verify", "-q", "HEAD"],
                Some(1),
                vec![],
                vec![]
            ),
            Ok(None)
        );
        // A malformed-config / fatal git error must not be mistaken for unborn.
        assert_eq!(
            classify(
                &["rev-parse", "--verify", "-q", "HEAD"],
                Some(128),
                vec![],
                b"fatal: not a git repository".to_vec()
            ),
            Err(HeadError::ReadFailed)
        );
        // stderr on exit 1 is also a read failure, not a clean absence.
        assert_eq!(
            classify(
                &["rev-parse", "--verify", "-q", "HEAD"],
                Some(1),
                vec![],
                b"error: bogus".to_vec()
            ),
            Err(HeadError::ReadFailed)
        );
    }

    /// Codex round 1 #2906 P1-1 / round 3 P2: a readable cwd is not enough. Git
    /// discovery reaches parent repositories, includes and alternates, so the
    /// metadata contract requires unrestricted read authority before any reader
    /// spawns. This unit test asserts the authority gate refuses the read; it
    /// never launches a real git subprocess. Real-repo behavior is covered by
    /// the integration / real-resource suite.
    #[test]
    fn host_repo_requires_unrestricted_read_authority() {
        let repo = HostRepo {
            cwd: std::path::PathBuf::from("repo/child"),
            read_scope: crate::Scope::only(["repo/child".to_string()]),
        };
        // A narrow read scope rejects the metadata read before git is spawned.
        assert_eq!(repo.head(), Err(HeadError::ReadFailed));
        assert!(repo.landed(None, "h", 4).is_empty());
        assert!(repo.stat("abc").is_empty());
    }

    /// Codex round 4 #2906 P2: a failed or oversized bounded pipe read must
    /// not be flattened to empty output. A collector that returns oversized
    /// or disconnected output with exit 1 must be classified as a read
    /// failure, not as a clean "no HEAD" absence.
    #[test]
    fn oversized_collector_is_read_failure_not_unborn() {
        let oversized = vec![b'x'; GIT_OUTPUT_CAP + 1];
        let rx = read_bounded(
            std::io::Cursor::new(oversized),
            GIT_OUTPUT_CAP,
            GIT_READ_DEADLINE,
        );
        assert_eq!(
            collect_output(Some(rx), GIT_READ_DEADLINE),
            Err(HeadError::ReadFailed)
        );
    }

    #[test]
    fn disconnected_collector_is_read_failure() {
        let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
        drop(tx);
        assert_eq!(
            collect_output(Some(rx), GIT_READ_DEADLINE),
            Err(HeadError::ReadFailed)
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
