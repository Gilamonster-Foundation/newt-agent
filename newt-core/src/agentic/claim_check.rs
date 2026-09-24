//! #867: file-path claim verification for the cap-exit summary.
//!
//! The #867 forensic session: the round cap hit, `trim_for_summary` dropped
//! the middle of the transcript, and the tools-disabled summary confidently
//! cited `newt-tui/src/commands.rs (lines 38-40)` — a file that does not
//! exist (the model pattern-completed a *typical* Rust repo layout once its
//! real evidence was trimmed away). The phantom-reach telemetry (#717)
//! covers hallucinated *tool* names; this module is its sibling for
//! hallucinated *file* names: extract path-like claims from the final text
//! and verify each against the workspace, appending a visible refutation for
//! anything that does not resolve. The model's prose is never rewritten —
//! the check only ever *appends* a clearly-marked annotation, so the user
//! sees exactly what the model said plus what did not check out.
//!
//! Pure by construction: extraction is string processing and resolution is an
//! injected seam, so the unit tier stays fully mocked.
//! Only the [`annotate_against_workspace`] wiring (called from every final
//! (no-tool-call) answer a turn produces — cap-exit summary and normal
//! finish alike, #1964) touches the real filesystem — and it never probes
//! lexically outside the workspace root: an absolute or `..`-escaping claim
//! is unverified, not missing, and is not probed. This lexical boundary does
//! not provide symlink isolation for paths that start inside the workspace.

/// Path-like tokens in assistant prose — the same recognition rule as the
/// crew planner's claim check (`newt-cli/src/crew.rs::path_tokens`): a token
/// containing a `/` with a short alphanumeric extension. Chat-prose
/// hardening on top of that rule: markdown emphasis/backtick wrapping and
/// `path:line` suffixes fall away via the split set, trailing sentence
/// punctuation is trimmed, URL-shaped tokens are skipped, and a token with
/// no letters (`1.2/3.4`) is not a claim. Order-preserving, deduplicated —
/// precision over recall, like the crew check.
pub(crate) fn path_claims(text: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || "()[]{}<>,;:\"'`*".contains(c)) {
        // Trailing-only trim: a leading `.` is load-bearing (`.newt/config.toml`,
        // `./src/lib.rs`) and must survive.
        let t = raw.trim_end_matches(|c: char| ".,!?".contains(c));
        // A URL is never a workspace claim. `:` is a split character, so
        // `https://host/a.rs` arrives here as the remnant `//host/a.rs` —
        // reject the protocol-relative shape, not just a literal `://`.
        if t.is_empty() || t.starts_with("//") || t.contains("://") {
            continue;
        }
        let has_ext = t.rsplit_once('.').is_some_and(|(_, ext)| {
            (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
        if t.contains('/')
            && has_ext
            && t.chars().any(|c| c.is_ascii_alphabetic())
            && seen.insert(t.to_string())
        {
            out.push(t.to_string());
        }
    }
    out
}

/// Cap on how many refuted paths the annotation lists verbatim — beyond it
/// the count is summarized, so a pathological summary can't bloat the reply.
const LISTED_CLAIMS: usize = 8;

/// Append distinct missing and unverified annotations, preserving the prose
/// exactly. `Some` is an in-workspace existence result; `None` means the
/// checker cannot inspect the claim. Both lists share the existing display cap.
fn annotate_path_claims(mut text: String, mut resolve: impl FnMut(&str) -> Option<bool>) -> String {
    let mut missing = Vec::new();
    let mut unverified = Vec::new();
    for claim in path_claims(&text) {
        match resolve(&claim) {
            Some(true) => {}
            Some(false) => missing.push(claim),
            None => unverified.push(claim),
        }
    }
    let mut remaining = LISTED_CLAIMS;
    for (claims, label) in [
        (missing, "not found in this workspace"),
        (unverified, "unverified outside workspace (not inspected)"),
    ] {
        if claims.is_empty() {
            continue;
        }
        let listed: Vec<_> = claims
            .iter()
            .take(remaining)
            .map(|p| format!("`{p}`"))
            .collect();
        remaining -= listed.len();
        let more = claims.len() - listed.len();
        let overflow = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        text = format!(
            "{text}\n\n⚠ claim check (#867): cited path(s) {label}: {}{overflow} \
             — verify these before acting on the summary above.",
            listed.join(", ")
        );
    }
    text
}

/// Bound on learned extra bases (below), mirroring [`OBSERVED_CAP`]'s reasoning:
/// a pathological turn must not make the resolver scan unboundedly.
const EXTRA_BASES_CAP: usize = 40;

/// The REAL-filesystem claim resolver for `workspace`: a claim resolves when
/// its lexically-normalized absolute form, joined either to the workspace
/// root OR to a "learned" extra base, stays inside the workspace AND exists
/// on disk. Claims that normalize outside the root (absolute paths
/// elsewhere, `..` escapes) are unverified without being probed. This is a
/// lexical fence, not symlink isolation. The [`ObservedPaths`] adapter below
/// records only positive existence results.
///
/// #1970: a bare relative fragment (`src/lib.rs`) cited alongside an earlier
/// claim that verified under a subdirectory (`agent-voice/agent-voice-tts/Cargo.toml`)
/// was refuted, even though the intended referent
/// (`agent-voice/agent-voice-tts/src/lib.rs`) exists — the resolver only
/// ever tried the workspace root. Every claim this closure verifies now
/// learns its parent directory as an extra base for claims checked *after*
/// it (citation order, capped at [`EXTRA_BASES_CAP`]), so a later bare
/// fragment resolves under the directory an earlier, fully-qualified claim
/// in the same text already established. A hit under more than one base is
/// still a verify, never a refutation — this checks existence, not identity.
fn workspace_claim_resolver(
    workspace: &str,
    mut exists: impl FnMut(&std::path::Path) -> bool,
) -> impl FnMut(&str) -> Option<bool> {
    let root = super::lexical_normalize(std::path::Path::new(workspace));
    let mut extra_bases: Vec<std::path::PathBuf> = Vec::new();
    move |claim: &str| {
        let p = std::path::Path::new(claim);
        if p.is_absolute() {
            let norm = super::lexical_normalize(p);
            return norm.starts_with(&root).then(|| exists(&norm));
        }
        let mut in_workspace = false;
        for base in std::iter::once(&root).chain(extra_bases.iter()) {
            let norm = super::lexical_normalize(&base.join(p));
            if !norm.starts_with(&root) {
                continue;
            }
            in_workspace = true;
            if exists(&norm) {
                if let Some(parent) = norm.parent().map(std::path::Path::to_path_buf) {
                    if extra_bases.len() < EXTRA_BASES_CAP && !extra_bases.contains(&parent) {
                        extra_bases.push(parent);
                    }
                }
                return Some(true);
            }
        }
        in_workspace.then_some(false)
    }
}

/// The observed-path ledger admits only claims that actually verified.
pub(crate) fn workspace_resolver(workspace: &str) -> impl FnMut(&str) -> bool {
    let mut resolve = workspace_claim_resolver(workspace, std::path::Path::exists);
    move |claim| resolve(claim) == Some(true)
}

/// Final-answer wiring (cap-exit AND normal finish): annotate `text` against
/// the real workspace tree without treating an uninspected claim as missing.
pub(crate) fn annotate_against_workspace(text: String, workspace: &str) -> String {
    annotate_path_claims(
        text,
        workspace_claim_resolver(workspace, std::path::Path::exists),
    )
}

/// Ledger cap: enough to name every file a real investigation touches while
/// keeping the cap-exit prompt bounded — collection stops once full.
const OBSERVED_CAP: usize = 40;

/// #867 Part A: the observed-paths ledger. Every tool-result round records
/// the path-like tokens that VERIFY against the workspace (grep's
/// `path:line:` hits, `find`/`list_dir` listings, …), deduplicated in
/// first-seen order and capped at [`OBSERVED_CAP`]. Collected as the rounds
/// happen, the ledger is immune to `trim_for_summary` — so the cap-exit
/// nudge can hand the model a manifest of REAL paths to cite even though the
/// evidence messages themselves were just trimmed away.
///
/// Only paths that verify are recorded: the ledger is a whitelist of ground
/// truth, never a channel for a tool error message (or the model's own
/// echoed hallucination) to smuggle a fake path into the prompt.
#[derive(Default)]
pub(crate) struct ObservedPaths {
    ordered: Vec<String>,
}

impl ObservedPaths {
    /// Record every claim in `text` that `exists` verifies, skipping
    /// duplicates; a no-op once the cap is reached.
    pub(crate) fn record(&mut self, text: &str, mut exists: impl FnMut(&str) -> bool) {
        for claim in path_claims(text) {
            if self.ordered.len() >= OBSERVED_CAP {
                return;
            }
            if exists(&claim) && !self.ordered.contains(&claim) {
                self.ordered.push(claim);
            }
        }
    }

    /// The recorded paths, first-seen order.
    pub(crate) fn into_vec(self) -> Vec<String> {
        self.ordered
    }
}

/// #1214: ground truth about the workspace's git state across THIS turn —
/// captured by the caller (HEAD at turn start vs. cap-exit) and handed to
/// [`annotate_action_claims`] as pure data, so the analysis stays in the
/// mocked unit tier. Collected at runtime by [`collect_git_evidence`].
pub(crate) struct TurnGitEvidence {
    /// HEAD moved during the turn — a commit was actually created.
    pub head_moved: bool,
    /// The working tree / index has uncommitted changes right now.
    pub tree_dirty: bool,
    /// Local branch names that exist right now.
    pub branches: Vec<String>,
}

/// Repo-relative path → two-character `git status --porcelain` code.
pub type StatusSnapshot = std::collections::BTreeMap<String, String>;

/// Parse `git status --porcelain=v1 -z` output. `-z` gives raw (unquoted) paths;
/// a rename/copy entry (`R`/`C`) is followed by its origin path, which is skipped.
fn parse_porcelain_z(out: &str) -> StatusSnapshot {
    let mut snapshot = StatusSnapshot::new();
    let mut entries = out.split('\0');
    while let Some(entry) = entries.next() {
        let (Some(code), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        if code.contains(['R', 'C']) {
            entries.next();
        }
        snapshot.insert(path.to_string(), code.to_string());
    }
    snapshot
}

/// Paths in `after` that are absent from `before` or carry a different status:
/// what the run changed, ignoring dirt that was there when it started. (A file
/// already dirty and dirty in the same way stays invisible — the probe is a
/// status delta, not a content diff.)
#[must_use]
pub fn files_changed_between(before: &StatusSnapshot, after: &StatusSnapshot) -> Vec<String> {
    after
        .iter()
        .filter(|(path, code)| before.get(*path) != Some(*code))
        .map(|(path, _)| path.clone())
        .collect()
}

/// The plain probe `collect_git_evidence` runs at every turn finalisation: it only
/// needs emptiness, so untracked directories stay collapsed (cheap on a big tree).
const EVIDENCE_STATUS_ARGS: [&str; 2] = ["status", "--porcelain"];

/// The hand-back snapshot: raw paths (`-z`) and every untracked FILE, so a stray
/// script inside a new directory is named, not hidden behind `dir/`.
const SNAPSHOT_STATUS_ARGS: [&str; 4] = ["status", "--porcelain=v1", "-z", "--untracked-files=all"];

/// One first-level subdirectory of a NON-repo workspace root that is its own
/// git repo (multi-repo recon PR2 — a bare folder of several checkouts).
/// `status` is `None` when the probe itself could not run — outside the
/// fs-read fence, or any git failure — so the caller can name the repo
/// without inventing a file list for it.
#[derive(Debug, Clone)]
pub struct NestedRepoSnapshot {
    /// The repo's directory name, relative to the workspace root (e.g.
    /// `"repoA"`) — the prefix `nested_files_changed_between` uses.
    pub repo: String,
    pub status: Option<StatusSnapshot>,
}

/// Does `dir` look like a git repo root — a `.git` entry directly inside it
/// (directory, or a worktree/submodule pointer file)? Deliberately NOT
/// `workspace_key.rs`'s `discover_git_dir`: that walks UP from a start point
/// and resolves worktree pointer files to their real gitdir, answering "is
/// this path INSIDE a repo"; this answers a narrower question — "is this
/// EXACT directory a repo root" — for filtering a directory LISTING, where
/// walking up would find the SAME repo from every subdirectory of a large
/// checkout and misclassify each as its own nested repo.
fn is_repo_root(dir: &std::path::Path) -> bool {
    dir.join(".git").exists()
}

/// Probe each first-level subdirectory of `workspace` that is its own git
/// repo — the bare-folder-of-checkouts case `snapshot_workspace` cannot see
/// (it only probes `workspace` itself). Reuses
/// [`crate::tooling::first_level_subdirs`] (never a second directory
/// lister). Only called by the caller when `workspace` itself is NOT a repo;
/// this function does not check that.
///
/// The fs-read fence is checked PER REPO via [`crate::caveats::permits_path`]
/// (prefix containment, the same check every other fs-read site uses) before
/// even attempting the probe. In practice `git_in`'s own gate
/// (`check_git_read_scope`'s "metadata" class) is coarser than this: a
/// BOUNDED `fs_read` (`Scope::Only`) refuses every metadata git read
/// uniformly, not per path — the SAME all-or-nothing rule
/// [`snapshot_workspace`] is already subject to today, verified by
/// `snapshot_nested_repos_names_every_repo_as_unprobed_under_a_bounded_scope`.
/// The `permits_path` check is kept anyway as the documented, narrower
/// intent (defense in depth against `git_in`'s gate ever becoming
/// path-aware) — either way, a repo whose probe does not succeed is
/// returned with `status: None` rather than silently skipped, so the
/// hand-back can name it as unprobed instead of making it invisible.
#[must_use]
pub fn snapshot_nested_repos(
    workspace: &str,
    read_scope: &crate::Scope<String>,
) -> Vec<NestedRepoSnapshot> {
    crate::tooling::first_level_subdirs(std::path::Path::new(workspace))
        .into_iter()
        // #2552 round 2 should-fix: `first_level_subdirs`' `is_dir()` and
        // `is_repo_root`'s `.exists()` both follow symlinks, so a first-level
        // entry that is a SYMLINK to an outside repo (`workspace/ext ->
        // /outside/repo`) would otherwise be probed and its paths/status
        // reported as `ext/…` — a leak of a repo outside the workspace
        // entirely. Filtered here (not in the shared lister, which has other
        // callers that may want symlinks) via `symlink_metadata`, which does
        // NOT follow the link, so a symlink is excluded before `is_repo_root`
        // ever resolves it.
        .filter(|dir| {
            dir.symlink_metadata()
                .is_ok_and(|meta| !meta.file_type().is_symlink())
        })
        .filter(|dir| is_repo_root(dir))
        .filter_map(|dir| {
            let repo = dir.file_name()?.to_string_lossy().into_owned();
            let dir_str = dir.to_string_lossy().into_owned();
            let status = crate::caveats::permits_path(read_scope, &dir_str)
                .then(|| git_in(&dir_str, &SNAPSHOT_STATUS_ARGS, read_scope))
                .flatten()
                .map(|out| parse_porcelain_z(&out));
            Some(NestedRepoSnapshot { repo, status })
        })
        .collect()
}

/// The nested-repo equivalent of [`files_changed_between`]: for each repo
/// present in `after` with a successful probe, diffed against the SAME repo
/// in `before` (matched by directory name) — `<repo>/<path>` for every
/// changed file, sorted. Returns `(files, unprobed)`: `unprobed` names every
/// repo excluded from `files` — a probe failure at either end, a repo
/// `after` found that `before` never saw (nothing trustworthy to diff
/// against), or a repo `before` saw that vanished by `after` (deleted or
/// renamed away mid-run — #2552 round 2 should-fix: previously silently
/// dropped instead of named) — so it is always reported, never silently
/// absorbed into an empty diff.
#[must_use]
pub fn nested_files_changed_between(
    before: &[NestedRepoSnapshot],
    after: &[NestedRepoSnapshot],
) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut unprobed = Vec::new();
    for repo_after in after {
        let Some(after_status) = &repo_after.status else {
            unprobed.push(repo_after.repo.clone());
            continue;
        };
        let before_status = before
            .iter()
            .find(|b| b.repo == repo_after.repo)
            .and_then(|b| b.status.as_ref());
        let Some(before_status) = before_status else {
            unprobed.push(repo_after.repo.clone());
            continue;
        };
        for path in files_changed_between(before_status, after_status) {
            files.push(format!("{}/{path}", repo_after.repo));
        }
    }
    for repo_before in before {
        if !after.iter().any(|a| a.repo == repo_before.repo) {
            unprobed.push(repo_before.repo.clone());
        }
    }
    files.sort();
    unprobed.sort();
    unprobed.dedup();
    (files, unprobed)
}

/// The nested-repo equivalent of a single repo's CURRENT dirty set (not a
/// delta) — every path in each successfully-probed repo's own status
/// snapshot, prefixed `<repo>/`. Returns `(files, unprobed)`, same shape and
/// same reason as [`nested_files_changed_between`]: a repo whose probe
/// failed is named, never silently absorbed into an empty list.
#[must_use]
pub fn nested_current_paths(snapshots: &[NestedRepoSnapshot]) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut unprobed = Vec::new();
    for repo in snapshots {
        match &repo.status {
            Some(status) => {
                for path in status.keys() {
                    files.push(format!("{}/{path}", repo.repo));
                }
            }
            None => unprobed.push(repo.repo.clone()),
        }
    }
    files.sort();
    (files, unprobed)
}

/// F26 v4 (#2552 round 3): the ONE decision every hand-back git source must
/// obey — where does `workspace` sit relative to a repo? `git rev-parse
/// --show-toplevel`, CANONICALIZED, compared against a CANONICALIZED
/// `workspace` (a symlinked spelling of the real root — macOS `/tmp` →
/// `/private/tmp`, or a symlinked checkout — must still read as
/// [`OwnRoot`](WorkspaceRepoLocation::OwnRoot)). Three-way, not the round-2
/// bool: round 2's `false` collapsed "not a repo at all" and "a subdirectory
/// of a larger repo" into one case and dropped subtree-scoped status
/// entirely for the second — round 3's review: that re-creates the exact
/// "nothing changed" bug this PR fixes for the everyday `--cwd repo/crate`
/// invocation, where `crate` has no nested repos of its own to fall back on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRepoLocation {
    /// `workspace` IS the repo toplevel — today's behaviour, unchanged.
    OwnRoot,
    /// `workspace` is a SUBDIRECTORY of a larger repo. `prefix` is `git
    /// rev-parse --show-prefix` (e.g. `"crate/"`) — the string every path
    /// reported for this case is stripped of, so it stays workspace-relative.
    InsideRepo { prefix: String },
    /// No repo found at all (or the read scope refused the probe).
    NotARepo,
}

/// See [`WorkspaceRepoLocation`]. Every caller (the shelled `snapshot_workspace`
/// / `snapshot_workspace_subtree` below, `newt_git::GitEngine` opened by
/// `headless.rs`, its commits/HEAD baseline, and the nested-repo probe's
/// gate) makes this SAME call, rather than independent (and, in #2552,
/// inconsistent) tests.
#[must_use]
pub fn locate_workspace_repo(
    workspace: &str,
    read_scope: &crate::Scope<String>,
) -> WorkspaceRepoLocation {
    let Some(toplevel) = git_in(workspace, &["rev-parse", "--show-toplevel"], read_scope) else {
        return WorkspaceRepoLocation::NotARepo;
    };
    let (Ok(toplevel), Ok(root)) = (
        std::fs::canonicalize(toplevel.trim()),
        std::fs::canonicalize(workspace),
    ) else {
        return WorkspaceRepoLocation::NotARepo;
    };
    if toplevel == root {
        return WorkspaceRepoLocation::OwnRoot;
    }
    let Some(prefix) = git_in(workspace, &["rev-parse", "--show-prefix"], read_scope) else {
        return WorkspaceRepoLocation::NotARepo;
    };
    WorkspaceRepoLocation::InsideRepo {
        prefix: prefix.trim().to_string(),
    }
}

/// Convenience for a caller that only needs the `OwnRoot` yes/no (e.g.
/// deciding whether the embedded `GitEngine`'s STATUS methods, never its
/// commits, are safe to use — see [`WorkspaceRepoLocation`]'s doc).
#[must_use]
pub fn is_workspace_repo_root(workspace: &str, read_scope: &crate::Scope<String>) -> bool {
    locate_workspace_repo(workspace, read_scope) == WorkspaceRepoLocation::OwnRoot
}

/// The workspace's changed-path snapshot when it IS a repo toplevel
/// ([`WorkspaceRepoLocation::OwnRoot`]); `None` otherwise (off-repo, or a
/// subdirectory of a larger repo — see [`snapshot_workspace_subtree`] for
/// that case instead, never an unscoped view of the enclosing repo).
#[must_use]
pub fn snapshot_workspace(
    workspace: &str,
    read_scope: &crate::Scope<String>,
) -> Option<StatusSnapshot> {
    if !is_workspace_repo_root(workspace, read_scope) {
        return None;
    }
    // Status alone: a repo with no commit yet has no HEAD, and must still be probed.
    let out = git_in(workspace, &SNAPSHOT_STATUS_ARGS, read_scope)?;
    Some(parse_porcelain_z(&out))
}

/// The workspace's changed-path snapshot when `workspace` is a SUBDIRECTORY
/// of a larger repo ([`WorkspaceRepoLocation::InsideRepo`]) — F26 v2's
/// scoping, restored in round 3 as a real (not dropped) third case: status
/// scoped with pathspec `.` (nothing outside the workspace subtree is even
/// considered) and every path stripped of `prefix` (`git rev-parse
/// --show-prefix`, from [`locate_workspace_repo`]) so it stays
/// workspace-relative. A nested git repo INSIDE the workspace collapses to a
/// single opaque `<dir>/` placeholder in that scoped status (git never
/// descends into an embedded repo) — dropped here, since
/// [`snapshot_nested_repos`] is the real way to see inside one.
#[must_use]
pub fn snapshot_workspace_subtree(
    workspace: &str,
    prefix: &str,
    read_scope: &crate::Scope<String>,
) -> Option<StatusSnapshot> {
    let mut args: Vec<&str> = SNAPSHOT_STATUS_ARGS.to_vec();
    args.extend(["--", "."]);
    let out = git_in(workspace, &args, read_scope)?;
    Some(
        parse_porcelain_z(&out)
            .into_iter()
            .filter_map(|(path, code)| {
                let rel = path.strip_prefix(prefix)?;
                (!rel.ends_with('/')).then(|| (rel.to_string(), code))
            })
            .collect(),
    )
}

/// `phrase` appears in `text` (already lowercased) with non-alphanumeric
/// boundaries on both sides — `contains` with word edges, no regex dep.
fn has_phrase(lower: &str, phrase: &str) -> bool {
    let mut from = 0;
    while let Some(i) = lower[from..].find(phrase) {
        let start = from + i;
        let end = start + phrase.len();
        let left_ok = start == 0
            || !lower[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let right_ok = end == lower.len()
            || !lower[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Completed-work phrases (#1214, from the live transcripts): claims of a
/// commit, push, opened PR, or passing tests/build. Conservative on purpose —
/// precision over recall, like the path check. Pure data.
const WORK_CLAIM_PHRASES: [&str; 12] = [
    "committed",
    "created a commit",
    "commit ahead",
    "commits ahead",
    "single commit",
    "pushed",
    "opened a pull request",
    "pull request created",
    "tests pass",
    "test passes",
    "tests passed",
    "check is green",
];

/// `true` when the summary claims a completed work product.
pub(crate) fn claims_completed_work(text: &str) -> bool {
    let lower = text.to_lowercase();
    WORK_CLAIM_PHRASES.iter().any(|p| has_phrase(&lower, p))
}

/// Branch names the summary claims: the first ref-looking token within a few
/// words of a `branch`/`branches` mention — the live transcripts say both
/// "branch `X`" and "Branch is clean on X". Ref-looking is strict (must
/// contain `/` or a digit, ref charset only), so hyphenated prose like
/// "tools-disabled" near the word "branch" is never mistaken for a claim —
/// precision over recall, like [`path_claims`]. Backtick/quote wrapping and
/// trailing punctuation fall away.
pub(crate) fn claimed_branches(text: &str) -> Vec<String> {
    const WINDOW: usize = 4;
    let mut out = Vec::new();
    let toks: Vec<&str> = text.split_whitespace().collect();
    for (i, tok) in toks.iter().enumerate() {
        let key = tok
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_lowercase();
        if key != "branch" && key != "branches" {
            continue;
        }
        for cand in toks.iter().skip(i + 1).take(WINDOW) {
            let cand = cand.trim_matches(|c: char| "`'\"()[],;:!?*".contains(c));
            let cand = cand.trim_end_matches('.');
            let refy = cand.contains('/') || cand.chars().any(|c| c.is_ascii_digit());
            if !cand.is_empty()
                && refy
                && cand
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c))
            {
                if !out.contains(&cand.to_string()) {
                    out.push(cand.to_string());
                }
                break; // one claim per mention; keep scanning after it
            }
        }
    }
    out
}

/// #1214: append refutations for claimed ACTIONS the workspace's git state
/// contradicts — the sibling of [`annotate_path_claims`] for work products
/// instead of paths. Same posture: append-only, prose preserved as an exact
/// prefix, no annotation when everything checks out (or no evidence exists —
/// a non-git workspace refutes nothing). Two checks:
/// - a claimed branch name that does not exist;
/// - a completed-work claim (commit / push / PR / tests pass) when HEAD did
///   not move this turn — with the working tree state deciding the wording
///   (clean tree = no work product exists at all; dirty = work exists but is
///   uncommitted, so commit-level claims are still false).
pub(crate) fn annotate_action_claims(text: String, evidence: Option<&TurnGitEvidence>) -> String {
    let Some(ev) = evidence else { return text };
    let mut notes: Vec<String> = Vec::new();
    for b in claimed_branches(&text) {
        if !ev.branches.iter().any(|have| have == &b) {
            notes.push(format!("claimed branch `{b}` does not exist"));
        }
    }
    if claims_completed_work(&text) && !ev.head_moved {
        notes.push(if ev.tree_dirty {
            "no commit was created this turn (HEAD unchanged) — changes exist but are \
             uncommitted, so commit/push/PR claims above are not true yet"
                .to_string()
        } else {
            "no commit was created this turn and the working tree is clean — the claimed \
             work product does not exist in this workspace"
                .to_string()
        });
    }
    if notes.is_empty() {
        return text;
    }
    format!(
        "{text}\n\n⚠ claim check (#1214): {} — verify the workspace state before \
         trusting the summary above.",
        notes.join("; ")
    )
}

/// Runtime evidence collector (the thin real-git wrapper around the pure
/// analysis; the two cap-exit sites call it). `head_at_turn_start` is the
/// [`git_head`] capture from the top of the turn. Any git failure (not a
/// repo, no git binary) yields `None` — no evidence, no refutation,
/// fail-quiet: this check must never break a summary.
pub(crate) fn collect_git_evidence(
    workspace: &str,
    read_scope: &crate::Scope<String>,
    head_at_turn_start: Option<&str>,
) -> Option<TurnGitEvidence> {
    let head_now = git_head(workspace, read_scope)?;
    let status = git_in(workspace, &EVIDENCE_STATUS_ARGS, read_scope)?;
    let branches = git_in(
        workspace,
        &["branch", "--format=%(refname:short)"],
        read_scope,
    )?
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect();
    Some(TurnGitEvidence {
        head_moved: head_at_turn_start.is_some_and(|start| start != head_now),
        tree_dirty: !status.trim().is_empty(),
        branches,
    })
}

/// Current HEAD sha of `workspace`, or `None` off-repo / on failure.
pub(crate) fn git_head(workspace: &str, read_scope: &crate::Scope<String>) -> Option<String> {
    git_in(workspace, &["rev-parse", "HEAD"], read_scope).map(|s| s.trim().to_string())
}

/// Run a read-only git plumbing command in `workspace`; `None` on any failure.
fn git_in(workspace: &str, args: &[&str], read_scope: &crate::Scope<String>) -> Option<String> {
    // Confused-deputy-safe: `workspace` may be a hostile repo whose `.git/config`
    // could turn a raw `git` read into out-of-fence code (core.fsmonitor, hooks,
    // diff.external, …). Metadata also requires explicit read authority.
    let out = crate::git_hardening::metadata_git(std::path::Path::new(workspace), args, read_scope)
        .ok()?
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2504 review: `collect_git_evidence` runs at every turn finalisation on every
    /// lane and only needs emptiness, so it must keep the cheap plain probe; only
    /// the hand-back snapshot enumerates untracked files.
    #[test]
    fn evidence_probe_stays_plain_and_only_the_snapshot_lists_untracked() {
        assert_eq!(EVIDENCE_STATUS_ARGS, ["status", "--porcelain"]);
        assert!(SNAPSHOT_STATUS_ARGS.contains(&"--untracked-files=all"));
        assert!(!EVIDENCE_STATUS_ARGS.contains(&"--untracked-files=all"));
    }

    #[test]
    fn porcelain_z_parses_spaces_untracked_and_renames() {
        let out = " M src/a b.rs\0?? new.txt\0R  moved.rs\0old.rs\0";
        let s = parse_porcelain_z(out);
        assert_eq!(s.get("src/a b.rs").map(String::as_str), Some(" M"));
        assert_eq!(s.get("new.txt").map(String::as_str), Some("??"));
        assert_eq!(s.get("moved.rs").map(String::as_str), Some("R "));
        assert!(
            !s.contains_key("old.rs"),
            "a rename origin is not a path of its own"
        );
        assert!(parse_porcelain_z("").is_empty());
    }

    /// U7: the delta ignores dirt that predates the run, but reports a path whose
    /// status changed, and a path that appeared.
    #[test]
    fn files_changed_is_the_delta_not_the_dirt() {
        let snap = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect::<StatusSnapshot>()
        };
        let before = snap(&[("dirty.txt", " M"), ("staged.txt", "A ")]);
        let after = snap(&[
            ("dirty.txt", " M"),
            ("staged.txt", "AM"),
            ("stray.sh", "??"),
        ]);
        assert_eq!(
            files_changed_between(&before, &after),
            ["staged.txt", "stray.sh"]
        );
        assert!(files_changed_between(&after, &after).is_empty());
    }

    /// The #867 transcript shapes: bold-wrapped path with a parenthesized
    /// line hint, a backticked `path:line`, a URL (never a claim), and a
    /// letterless version-ish token (never a claim).
    #[test]
    fn path_claims_extracts_slash_ext_tokens_from_chat_prose() {
        let text = "Defined in **newt-tui/src/commands.rs** (lines 38-40); \
                    handler at `session/mod.rs:567`. See \
                    https://github.com/x/y/blob/main/z.rs and spec 1.2/3.4.";
        assert_eq!(
            path_claims(text),
            vec!["newt-tui/src/commands.rs", "session/mod.rs"]
        );
    }

    #[test]
    fn path_claims_keeps_leading_dots_dedupes_and_skips_prose() {
        let text = "check .newt/config.toml then ./src/lib.rs, then .newt/config.toml again. \
                    A plain sentence, an edit/shell mention, and lib.rs alone are not claims.";
        assert_eq!(path_claims(text), vec![".newt/config.toml", "./src/lib.rs"]);
        assert!(path_claims("no paths here at all").is_empty());
    }

    #[test]
    fn annotate_is_a_noop_when_claims_verify_or_are_absent() {
        let clean = "all good, nothing cited".to_string();
        assert_eq!(annotate_path_claims(clean.clone(), |_| Some(false)), clean);
        let cited = "the fix is in a/b.rs and c/d.rs".to_string();
        assert_eq!(annotate_path_claims(cited.clone(), |_| Some(true)), cited);
    }

    #[test]
    fn annotate_appends_refutation_and_preserves_the_prose_prefix() {
        let cited = "the fix is in a/b.rs and c/d.rs".to_string();
        let out = annotate_path_claims(cited.clone(), |c| Some(c == "a/b.rs"));
        assert!(out.starts_with(&cited), "prose must be an exact prefix");
        assert!(out.contains("⚠ claim check (#867)"), "got: {out}");
        assert!(out.contains("`c/d.rs`"), "the missing path is named");
        assert!(!out.contains("`a/b.rs`"), "verified paths are not listed");
    }

    #[test]
    fn annotate_caps_the_listed_paths() {
        let cited: String = (0..12)
            .map(|i| format!("see dir{i}/f{i}.rs "))
            .collect::<String>();
        let out = annotate_path_claims(cited, |_| Some(false));
        assert!(out.contains("`dir0/f0.rs`"));
        assert!(out.contains("`dir7/f7.rs`"));
        assert!(!out.contains("`dir8/f8.rs`"), "capped at {LISTED_CLAIMS}");
        assert!(out.contains("(+4 more)"), "got: {out}");
    }

    /// #867 Part A: the ledger records only verified paths, dedupes in
    /// first-seen order, and stops at the cap — an error message citing a
    /// fake path can never enter the manifest.
    #[test]
    fn observed_paths_records_verified_dedupes_and_caps() {
        let mut led = ObservedPaths::default();
        led.record("src/a.rs:12: hit and src/b.rs:9: hit", |c| c != "src/b.rs");
        led.record("src/a.rs:44: again, plus docs/x.md", |_| true);
        assert_eq!(led.into_vec(), vec!["src/a.rs", "docs/x.md"]);

        let mut full = ObservedPaths::default();
        let many: String = (0..50).map(|i| format!("d/f{i}.rs ")).collect();
        full.record(&many, |_| true);
        let v = full.into_vec();
        assert_eq!(v.len(), 40, "capped at OBSERVED_CAP");
        assert_eq!(v[0], "d/f0.rs");
        assert_eq!(v[39], "d/f39.rs");
    }

    fn evidence(head_moved: bool, tree_dirty: bool, branches: &[&str]) -> TurnGitEvidence {
        TurnGitEvidence {
            head_moved,
            tree_dirty,
            branches: branches.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// #1214, from the live Ornith transcript: "Branch is clean on
    /// step-09.Help-rollup-for-the-548 (only my single commit ahead of
    /// bench/548-base)" — on a clean, unmoved workspace both the phantom
    /// branch and the phantom commit are refuted. Fails on the pre-fix code
    /// (no action check existed).
    #[test]
    fn refutes_phantom_branch_and_commit_from_the_live_transcript() {
        let text = "Branch is clean on step-09.Help-rollup-for-the-548 \
                    (only my single commit ahead of bench/548-base). \
                    All existing tests pass."
            .to_string();
        let ev = evidence(false, false, &["bench/548-base", "main"]);
        let out = annotate_action_claims(text.clone(), Some(&ev));
        assert!(out.starts_with(&text), "prose is an exact prefix");
        assert!(out.contains("⚠ claim check (#1214)"), "got: {out}");
        assert!(
            out.contains("`step-09.Help-rollup-for-the-548` does not exist"),
            "phantom branch refuted: {out}"
        );
        assert!(
            out.contains("working tree is clean"),
            "phantom work product refuted: {out}"
        );
        // The REAL branch is not refuted.
        assert!(!out.contains("`bench/548-base` does not exist"), "{out}");
    }

    /// True work is never refuted: HEAD moved → no annotation even with
    /// commit/test claims; and a claim-free summary is untouched regardless.
    #[test]
    fn honest_summaries_pass_untouched() {
        let honest = "committed the fix on branch fix/x-1; tests pass".to_string();
        let ev = evidence(true, false, &["fix/x-1", "main"]);
        assert_eq!(annotate_action_claims(honest.clone(), Some(&ev)), honest);

        let no_claims = "I explored the code and here is my analysis".to_string();
        let ev = evidence(false, false, &["main"]);
        assert_eq!(
            annotate_action_claims(no_claims.clone(), Some(&ev)),
            no_claims
        );
        // No evidence (not a git workspace) → never annotate.
        let claimy = "committed and pushed".to_string();
        assert_eq!(annotate_action_claims(claimy.clone(), None), claimy);
    }

    /// Uncommitted-but-real work gets the precise wording: the work exists,
    /// the commit-level claims are still false.
    #[test]
    fn dirty_tree_with_unmoved_head_gets_the_uncommitted_wording() {
        let text = "I committed the change".to_string();
        let ev = evidence(false, true, &["main"]);
        let out = annotate_action_claims(text, Some(&ev));
        assert!(out.contains("changes exist but are uncommitted"), "{out}");
    }

    /// Detection edges: word boundaries (no "repushed" match), prose after
    /// "branch" is not a ref, backticked refs unwrap.
    #[test]
    fn claim_detection_is_conservative() {
        assert!(claims_completed_work("we PUSHED the branch"));
        assert!(!claims_completed_work("the cap repushed my schedule"));
        assert!(claims_completed_work("all tests pass now"));
        assert!(!claims_completed_work("the test passage was unclear"));
        assert_eq!(
            claimed_branches("on branch `fix/a-1` and branch main stays; the branch is fine"),
            vec!["fix/a-1"],
            "prose words and bare names without ref-chars are not claims"
        );
    }

    /// The workspace wiring honors the fence: a `..` escape and an absolute
    /// path outside the root are refuted without a stat; a real in-tree file
    /// verifies. Uses this crate's own source tree read-only — no tempdirs,
    /// no writes.
    #[test]
    fn workspace_wiring_fences_and_resolves() {
        let ws = env!("CARGO_MANIFEST_DIR");
        let ok = annotate_against_workspace("see src/lib.rs".to_string(), ws);
        assert!(!ok.contains("⚠ claim check"), "real file verifies: {ok}");
        let bad = annotate_against_workspace(
            "see src/nope.rs and ../escape/x.rs and /etc/hosts.d/y.rs".to_string(),
            ws,
        );
        assert!(bad.contains("`src/nope.rs`"), "got: {bad}");
        assert!(bad.contains("`../escape/x.rs`"), "escape refuted: {bad}");
        assert!(
            bad.contains("`/etc/hosts.d/y.rs`"),
            "outside refuted: {bad}"
        );
    }

    /// Real existing and absent external files ground the annotation boundary:
    /// neither can be declared missing or verified by a workspace-only checker.
    #[test]
    fn external_path_claims_are_unverified_not_missing() {
        let tree = tempfile::tempdir().unwrap();
        let workspace = tree.path().join("workspace");
        let reports = tree.path().join("reports");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&reports).unwrap();
        let existing = reports.join("report.md");
        std::fs::write(&existing, "observed report").unwrap();
        // The existing prose tokenizer recognizes slash paths, not Windows
        // drive prefixes. Resolver boundary coverage below is platform-neutral.
        let absolute = cfg!(unix).then(|| existing.to_string_lossy().into_owned());
        for claim in absolute
            .as_deref()
            .into_iter()
            .chain(["../reports/report.md", "../reports/absent.md"])
        {
            let text = format!("See `{claim}`.");
            let out = annotate_against_workspace(text.clone(), workspace.to_str().unwrap());
            let annotation = out.strip_prefix(&text).expect("preserve original prose");
            assert!(
                annotation.contains("unverified outside workspace"),
                "external existence is not known to this checker: {out}"
            );
            assert!(!annotation.contains("not found"), "{out}");
            assert!(annotation.contains(&format!("`{claim}`")), "{out}");
        }
    }

    #[test]
    fn mixed_path_claims_keep_missing_and_unverified_separate() {
        let workspace = env!("CARGO_MANIFEST_DIR");
        let text = "See src/lib.rs, src/absent-file.rs and ../../reports/report.md.".to_string();
        let out = annotate_against_workspace(text.clone(), workspace);
        let annotation = out.strip_prefix(&text).expect("preserve original prose");
        let (missing, unverified) = annotation
            .split_once("unverified outside workspace")
            .expect("external claims have a distinct annotation");
        assert!(missing.contains("not found in this workspace"), "{out}");
        assert!(missing.contains("`src/absent-file.rs`"), "{out}");
        assert!(!missing.contains("`../../reports/report.md`"), "{out}");
        assert!(unverified.contains("`../../reports/report.md`"), "{out}");
        assert!(!annotation.contains("`src/lib.rs`"), "{out}");
    }

    #[test]
    fn external_path_annotation_keeps_the_existing_list_bound() {
        let text: String = (0..12)
            .map(|i| format!("See ../reports/report{i}.md. "))
            .collect();
        let out = annotate_against_workspace(text.clone(), env!("CARGO_MANIFEST_DIR"));
        let annotation = out.strip_prefix(&text).expect("preserve original prose");
        assert!(annotation.contains("unverified outside workspace"), "{out}");
        assert!(annotation.contains("`../reports/report0.md`"), "{out}");
        assert!(annotation.contains("`../reports/report7.md`"), "{out}");
        assert!(!annotation.contains("`../reports/report8.md`"), "{out}");
        assert!(annotation.contains("(+4 more)"), "{out}");
    }

    #[test]
    fn unverified_external_claims_do_not_enter_observed_paths() {
        let mut observed = ObservedPaths::default();
        observed.record(
            "See src/lib.rs and ../../reports/report.md.",
            workspace_resolver(env!("CARGO_MANIFEST_DIR")),
        );
        assert_eq!(observed.into_vec(), vec!["src/lib.rs"]);
    }

    #[test]
    fn workspace_claim_resolver_never_probes_lexical_escapes() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let outside = workspace.parent().unwrap().join("external-report.md");
        let mut probes = Vec::new();
        {
            let mut resolve = workspace_claim_resolver(workspace.to_str().unwrap(), |path| {
                probes.push(path.to_path_buf());
                true
            });
            assert_eq!(resolve(outside.to_str().unwrap()), None);
            assert_eq!(resolve("../../reports/report.md"), None);
            assert_eq!(resolve("src/lib.rs"), Some(true));
            // Learning src/ must not permit an escape beyond that base either.
            assert_eq!(resolve("../../reports/report.md"), None);
        }
        assert_eq!(probes, vec![workspace.join("src/lib.rs")]);
    }

    #[test]
    fn missing_and_unverified_claims_share_one_display_cap() {
        let text: String = (0..12).map(|i| format!("See dir/file{i}.md. ")).collect();
        let out = annotate_path_claims(text.clone(), |claim| {
            claim.ends_with("0.md").then_some(false)
        });
        let annotation = out.strip_prefix(&text).unwrap();
        assert_eq!(annotation.matches('`').count(), LISTED_CLAIMS * 2);
        assert!(annotation.contains("not found in this workspace"));
        assert!(annotation.contains("unverified outside workspace"));
        assert!(annotation.contains("(+4 more)"));
    }

    /// #1970 regression, reproduced against this repo's own workspace root
    /// (the parent of `CARGO_MANIFEST_DIR`, which contains sibling crates —
    /// the same shape as the reported bug's `agent-voice/agent-voice-tts/`
    /// subproject under `Gilamonster-Foundation`): `newt-core/Cargo.toml`
    /// verifies directly at root and its directory is learned; the bare
    /// fragment `src/lib.rs` cited right after it must now resolve there
    /// too, instead of being refuted as absent at the workspace root.
    #[test]
    fn bare_fragment_resolves_under_an_earlier_verified_claims_directory() {
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("newt-core has a workspace root parent");
        let ws = ws.to_str().expect("workspace root is valid UTF-8");
        let text = "see newt-core/Cargo.toml and also src/lib.rs".to_string();
        let out = annotate_against_workspace(text.clone(), ws);
        assert_eq!(out, text, "both claims verify, no annotation: {out}");
    }

    /// Twin of the above: a fragment that is genuinely absent everywhere —
    /// root and every learned base — still refutes. A false positive would
    /// be worse than the false refutation this fix closes (#1970's own
    /// framing: it "teaches operators to ignore refutations").
    #[test]
    fn a_genuinely_absent_fragment_still_refutes_even_with_a_learned_base() {
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("newt-core has a workspace root parent");
        let ws = ws.to_str().expect("workspace root is valid UTF-8");
        let text = "see newt-core/Cargo.toml and also nope/nope.rs".to_string();
        let out = annotate_against_workspace(text, ws);
        assert!(out.contains("⚠ claim check (#867)"), "got: {out}");
        assert!(out.contains("`nope/nope.rs`"), "got: {out}");
    }

    /// A claim resolving under more than one base (root AND a learned base)
    /// is still a verify — ambiguity is never treated as a refutation.
    #[test]
    fn a_claim_resolvable_under_multiple_bases_still_verifies() {
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("newt-core has a workspace root parent");
        let ws = ws.to_str().expect("workspace root is valid UTF-8");
        // `newt-core/Cargo.toml` verifies at root and learns `newt-core/` as
        // a base; `Cargo.toml` alone then resolves at BOTH the root (the
        // workspace's own Cargo.toml) and the learned `newt-core/` base.
        let text = "see newt-core/Cargo.toml and also Cargo.toml".to_string();
        let out = annotate_against_workspace(text.clone(), ws);
        assert_eq!(out, text, "ambiguous-but-real is still a verify: {out}");
    }

    /// Multi-repo recon PR2 fixtures: a `git()` runner scoped to `dir`, and a
    /// bare tempdir root holding two independent git repos (`repoA`,
    /// `repoB`), one with a real uncommitted edit — the exact r9-r12 shape
    /// (a bare folder of several checkouts) `snapshot_workspace` alone
    /// cannot see into.
    fn git_in_dir(dir: &std::path::Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git")
                .status
                .success(),
            "git {args:?} in {}",
            dir.display()
        );
    }

    fn init_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_in_dir(dir, &["init", "-q"]);
        git_in_dir(dir, &["config", "user.email", "t@example.com"]);
        git_in_dir(dir, &["config", "user.name", "t"]);
    }

    /// A bare root holding `repoA` (committed file `a.txt`, then edited
    /// uncommitted) and `repoB` (clean).
    fn bare_multi_repo_fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("bare multi-repo root");
        let repo_a = root.path().join("repoA");
        init_repo(&repo_a);
        std::fs::write(repo_a.join("a.txt"), "one\n").unwrap();
        git_in_dir(&repo_a, &["add", "a.txt"]);
        git_in_dir(&repo_a, &["commit", "-q", "-m", "init"]);
        std::fs::write(repo_a.join("a.txt"), "one\ntwo\n").unwrap(); // uncommitted edit
        init_repo(&root.path().join("repoB"));
        root
    }

    /// F24-recon row 5 / PR2 (red first): the bare-root shape reports EACH
    /// nested repo, never a bare "unavailable". `repoA`'s uncommitted edit
    /// is visible in its own status snapshot; `repoB` (clean) is present
    /// with an empty one.
    #[test]
    fn snapshot_nested_repos_probes_each_first_level_git_child() {
        let root = bare_multi_repo_fixture();
        assert!(
            snapshot_workspace(&root.path().to_string_lossy(), &crate::Scope::All).is_none(),
            "the bare root itself is not a repo"
        );
        let snapshots = snapshot_nested_repos(&root.path().to_string_lossy(), &crate::Scope::All);
        assert_eq!(snapshots.len(), 2, "{snapshots:?}");
        let repo_a = snapshots
            .iter()
            .find(|s| s.repo == "repoA")
            .expect("repoA probed");
        let status_a = repo_a.status.as_ref().expect("repoA's probe must succeed");
        assert_eq!(status_a.get("a.txt").map(String::as_str), Some(" M"));
        let repo_b = snapshots
            .iter()
            .find(|s| s.repo == "repoB")
            .expect("repoB probed");
        assert_eq!(
            repo_b
                .status
                .as_ref()
                .expect("repoB's probe must succeed")
                .len(),
            0,
            "repoB is clean"
        );
    }

    /// A plain (non-git) first-level subdirectory is not a candidate at all
    /// — never listed, never reported as unprobed. Only a `.git`-bearing
    /// directory counts as a nested repo.
    #[test]
    fn snapshot_nested_repos_ignores_non_repo_subdirs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("just_a_folder")).unwrap();
        let snapshots = snapshot_nested_repos(&root.path().to_string_lossy(), &crate::Scope::All);
        assert!(snapshots.is_empty(), "{snapshots:?}");
    }

    /// A nested repo that fails to probe (outside the fs-read fence, or any
    /// other `git_in` failure) is named, not silently skipped — `status:
    /// None`, distinguishable from "not a repo at all" (which never appears
    /// in the list). Measured: `check_git_read_scope`'s "metadata" class
    /// (what `git_in` uses) is all-or-nothing on `Scope::Only` — the SAME
    /// gate `snapshot_workspace` is already subject to today — so a bounded
    /// scope refuses every nested repo's probe uniformly, not per-path; this
    /// pins that behaviour is inherited, not silently different, for the
    /// nested case.
    #[test]
    fn snapshot_nested_repos_names_every_repo_as_unprobed_under_a_bounded_scope() {
        let root = bare_multi_repo_fixture();
        let scope = crate::Scope::only([root.path().to_string_lossy().into_owned()]);
        let snapshots = snapshot_nested_repos(&root.path().to_string_lossy(), &scope);
        assert_eq!(snapshots.len(), 2, "{snapshots:?}");
        for repo in &snapshots {
            assert!(
                repo.status.is_none(),
                "{}: a bounded fs_read scope refuses every metadata git read, \
                 the same as snapshot_workspace — named, not probed",
                repo.repo
            );
        }
        // Confirms the SAME gate the single-repo path is already subject to,
        // not a nested-only regression.
        assert!(snapshot_workspace(&root.path().join("repoA").to_string_lossy(), &scope).is_none());
    }

    /// `nested_files_changed_between` (the delta): repoA's edit, made AFTER
    /// `before` was captured, is reported prefixed `repoA/`; repoB (clean at
    /// both ends) contributes nothing.
    #[test]
    fn nested_files_changed_between_prefixes_by_repo_and_ignores_clean_repos() {
        let root = tempfile::tempdir().unwrap();
        let repo_a = root.path().join("repoA");
        init_repo(&repo_a);
        std::fs::write(repo_a.join("a.txt"), "one\n").unwrap();
        git_in_dir(&repo_a, &["add", "a.txt"]);
        git_in_dir(&repo_a, &["commit", "-q", "-m", "init"]);
        init_repo(&root.path().join("repoB"));

        let workspace = root.path().to_string_lossy().into_owned();
        let before = snapshot_nested_repos(&workspace, &crate::Scope::All);
        std::fs::write(repo_a.join("a.txt"), "one\ntwo\n").unwrap();
        let after = snapshot_nested_repos(&workspace, &crate::Scope::All);
        let (files, unprobed) = nested_files_changed_between(&before, &after);
        assert_eq!(files, vec!["repoA/a.txt".to_string()], "{files:?}");
        assert!(unprobed.is_empty(), "{unprobed:?}");
    }

    /// `nested_current_paths` (the CURRENT set, not a delta): repoA's
    /// uncommitted edit is visible with no "before" needed at all — this is
    /// what feeds `uncommitted_files` in the nested case.
    #[test]
    fn nested_current_paths_lists_every_probed_repos_dirty_files() {
        let root = bare_multi_repo_fixture();
        let snapshots = snapshot_nested_repos(&root.path().to_string_lossy(), &crate::Scope::All);
        let (files, unprobed) = nested_current_paths(&snapshots);
        assert_eq!(files, vec!["repoA/a.txt".to_string()], "{files:?}");
        assert!(unprobed.is_empty(), "{unprobed:?}");
    }

    /// A root that IS a repo takes the existing `snapshot_workspace` path
    /// untouched — `snapshot_nested_repos` is simply never reached by the
    /// caller in that case (the byte-identical-to-today claim is pinned at
    /// the `headless_contract`/`headless_cli` level, where the actual
    /// `files_changed_source` string is asserted); this test only confirms
    /// the pure claim_check-level behaviour is unaffected: `snapshot_workspace`
    /// still succeeds on a repo root exactly as before this PR touched
    /// anything.
    #[test]
    fn a_repo_root_still_probes_via_snapshot_workspace_unaffected_by_nesting() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        assert!(snapshot_workspace(&root.path().to_string_lossy(), &crate::Scope::All).is_some());
    }

    /// F26 v3 / #2552 round 2 (red first): a plain folder with no `.git` of
    /// its own, sitting INSIDE a larger git repo, is NOT its own repo root —
    /// `snapshot_workspace` must return `None` (never the enclosing repo's
    /// status, scoped or otherwise: `is_workspace_repo_root` is the single
    /// decision every hand-back source now obeys), and `snapshot_nested_repos`
    /// is the mechanism that actually reports the nested repo it contains.
    #[test]
    fn snapshot_workspace_never_discovers_an_enclosing_repo_above_the_workspace() {
        let outer = tempfile::tempdir().expect("outer root");
        init_repo(outer.path());
        std::fs::write(outer.path().join("outer.txt"), "outer\n").unwrap();
        git_in_dir(outer.path(), &["add", "outer.txt"]);
        git_in_dir(outer.path(), &["commit", "-q", "-m", "outer init"]);

        // `workspace`: a plain subfolder of the outer repo, itself not a
        // repo, holding one nested repo (`inner`).
        let workspace = outer.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let inner = workspace.join("inner");
        init_repo(&inner);
        std::fs::write(inner.join("i.txt"), "one\n").unwrap();
        git_in_dir(&inner, &["add", "i.txt"]);
        git_in_dir(&inner, &["commit", "-q", "-m", "inner init"]);
        std::fs::write(inner.join("i.txt"), "one\ntwo\n").unwrap(); // uncommitted

        let workspace_str = workspace.to_string_lossy().into_owned();
        assert!(
            !is_workspace_repo_root(&workspace_str, &crate::Scope::All),
            "a subfolder of a larger repo is not its own repo root"
        );
        assert!(
            snapshot_workspace(&workspace_str, &crate::Scope::All).is_none(),
            "must never resolve to the enclosing repo's status"
        );
        let snapshots = snapshot_nested_repos(&workspace_str, &crate::Scope::All);
        assert_eq!(snapshots.len(), 1, "{snapshots:?}");
        assert_eq!(snapshots[0].repo, "inner");
        let status = snapshots[0]
            .status
            .as_ref()
            .expect("inner's probe must succeed");
        assert_eq!(status.get("i.txt").map(String::as_str), Some(" M"));
    }

    /// #2552 round 2: the symlinked-root nit — `is_workspace_repo_root`
    /// canonicalizes BOTH sides, so a symlinked spelling of a real repo root
    /// still reads as "own repo", not as "a subdirectory of the same
    /// toplevel" (which `git rev-parse --show-toplevel`'s always-resolved
    /// answer would otherwise make a lexical comparison conclude).
    #[test]
    #[cfg(unix)]
    fn is_workspace_repo_root_true_through_a_symlinked_spelling_of_the_root() {
        let real = tempfile::tempdir().expect("real root");
        init_repo(real.path());
        let parent = real.path().parent().expect("tempdir has a parent");
        let link = parent.join(format!(
            "symlink-{}",
            real.path().file_name().unwrap().to_string_lossy()
        ));
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");
        assert!(
            is_workspace_repo_root(&link.to_string_lossy(), &crate::Scope::All),
            "a symlinked spelling of the real root must still count as its own repo"
        );
        std::fs::remove_file(&link).ok();
    }

    /// #2552 round 2 should-fix (red first): a first-level entry that is a
    /// SYMLINK to a repo OUTSIDE the workspace must never be probed —
    /// `first_level_subdirs`' `is_dir()` and `is_repo_root`'s `.exists()`
    /// both follow symlinks, so without an explicit skip this would report
    /// an outside repo's files under `ext/…`.
    #[test]
    #[cfg(unix)]
    fn snapshot_nested_repos_skips_a_symlink_to_an_outside_repo() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside repo");
        init_repo(outside.path());
        std::fs::write(outside.path().join("secret.txt"), "leak\n").unwrap();
        git_in_dir(outside.path(), &["add", "secret.txt"]);
        git_in_dir(outside.path(), &["commit", "-q", "-m", "init"]);
        std::fs::write(outside.path().join("secret.txt"), "leak\nmore\n").unwrap();

        let link = workspace.path().join("ext");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");

        let snapshots =
            snapshot_nested_repos(&workspace.path().to_string_lossy(), &crate::Scope::All);
        assert!(
            snapshots.is_empty(),
            "a symlinked first-level entry must never be probed: {snapshots:?}"
        );
    }

    /// #2552 round 2 should-fix (red first): a repo present at `before` but
    /// gone by `after` (deleted or renamed away mid-run) must be NAMED in
    /// `unprobed`, never silently dropped from the diff.
    #[test]
    fn nested_files_changed_between_names_a_repo_deleted_during_the_run() {
        let before = vec![NestedRepoSnapshot {
            repo: "gone".to_string(),
            status: Some(StatusSnapshot::new()),
        }];
        let after: Vec<NestedRepoSnapshot> = Vec::new();
        let (files, unprobed) = nested_files_changed_between(&before, &after);
        assert!(files.is_empty(), "{files:?}");
        assert_eq!(unprobed, vec!["gone".to_string()], "{unprobed:?}");
    }
}
