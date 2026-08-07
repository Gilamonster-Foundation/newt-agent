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
//! Pure by construction: extraction is string processing and existence is an
//! injected `Fn(&str) -> bool` seam, so the unit tier stays fully mocked.
//! Only the [`annotate_against_workspace`] wiring (called from the two
//! cap-exit sites in `mod.rs`) touches the real filesystem — and it never
//! probes outside the workspace root: the fs fence applies to the checker
//! too, so an absolute or `..`-escaping claim is reported as not-found
//! rather than stat'd.

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

/// The subset of [`path_claims`] that `exists` refutes, in citation order.
pub(crate) fn missing_claims(text: &str, exists: impl Fn(&str) -> bool) -> Vec<String> {
    path_claims(text)
        .into_iter()
        .filter(|c| !exists(c))
        .collect()
}

/// Cap on how many refuted paths the annotation lists verbatim — beyond it
/// the count is summarized, so a pathological summary can't bloat the reply.
const LISTED_CLAIMS: usize = 8;

/// Append the claim-check refutation to `text` when any cited path fails to
/// resolve; return `text` unchanged (no annotation, no trailing noise) when
/// every claim checks out or there are no claims at all. The original prose
/// is always preserved as an exact prefix — the check labels, never rewrites.
pub(crate) fn annotate_missing_claims(text: String, exists: impl Fn(&str) -> bool) -> String {
    let missing = missing_claims(&text, exists);
    if missing.is_empty() {
        return text;
    }
    let listed: Vec<String> = missing
        .iter()
        .take(LISTED_CLAIMS)
        .map(|p| format!("`{p}`"))
        .collect();
    let more = missing.len().saturating_sub(LISTED_CLAIMS);
    let overflow = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    format!(
        "{text}\n\n⚠ claim check (#867): cited path(s) not found in this workspace: {}{overflow} \
         — verify these before acting on the summary above.",
        listed.join(", ")
    )
}

/// The REAL-filesystem claim resolver for `workspace`: a claim resolves when
/// its lexically-normalized absolute form stays inside the workspace AND
/// exists on disk. Claims that normalize outside the root (absolute paths
/// elsewhere, `..` escapes) are refuted without ever being stat'd — the
/// checker honors the same workspace fence as the fs tools. Shared by the
/// cap-exit annotation and the [`ObservedPaths`] ledger.
pub(crate) fn workspace_resolver(workspace: &str) -> impl Fn(&str) -> bool {
    let root = super::lexical_normalize(std::path::Path::new(workspace));
    move |claim: &str| {
        let p = std::path::Path::new(claim);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        };
        let norm = super::lexical_normalize(&abs);
        norm.starts_with(&root) && norm.exists()
    }
}

/// Cap-exit wiring: annotate `text` against the REAL workspace tree via
/// [`workspace_resolver`].
pub(crate) fn annotate_against_workspace(text: String, workspace: &str) -> String {
    annotate_missing_claims(text, workspace_resolver(workspace))
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
    pub(crate) fn record(&mut self, text: &str, exists: impl Fn(&str) -> bool) {
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
/// contradicts — the sibling of [`annotate_missing_claims`] for work products
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
    head_at_turn_start: Option<&str>,
) -> Option<TurnGitEvidence> {
    let head_now = git_head(workspace)?;
    let status = git_in(workspace, &["status", "--porcelain"])?;
    let branches = git_in(workspace, &["branch", "--format=%(refname:short)"])?
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
pub(crate) fn git_head(workspace: &str) -> Option<String> {
    git_in(workspace, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// Run a read-only git plumbing command in `workspace`; `None` on any failure.
fn git_in(workspace: &str, args: &[&str]) -> Option<String> {
    // Confused-deputy-safe: `workspace` may be a hostile repo whose `.git/config`
    // could turn a raw `git` read into out-of-fence code (core.fsmonitor, hooks,
    // diff.external, …). `hardened_git` disarms that surface (step-7.4).
    let out = crate::git_hardening::hardened_git(std::path::Path::new(workspace), args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(annotate_missing_claims(clean.clone(), |_| false), clean);
        let cited = "the fix is in a/b.rs and c/d.rs".to_string();
        assert_eq!(annotate_missing_claims(cited.clone(), |_| true), cited);
    }

    #[test]
    fn annotate_appends_refutation_and_preserves_the_prose_prefix() {
        let cited = "the fix is in a/b.rs and c/d.rs".to_string();
        let out = annotate_missing_claims(cited.clone(), |c| c == "a/b.rs");
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
        let out = annotate_missing_claims(cited, |_| false);
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
}
