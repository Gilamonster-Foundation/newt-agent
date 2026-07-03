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
