//! The first-run / `newt setup` **standard caveat pack** (#2660) — common
//! text tools that can't run other programs (+ the configured backend's
//! host) as reviewable,
//! unsigned `approve.toml` candidates, built with the SAME gap/danger-
//! partition engine [`crate::ocap_propose::propose_from_capture`] already
//! uses for a flight-recorder observation. There is no second policy format
//! and no second danger table here: a candidate pool (pure data, below) is
//! fed through a SYNTHETIC [`crate::flight_recorder::FlightCapture`] that is
//! never written to disk, so the exact same gap-dedup and danger-defer logic
//! a `--full-access` session's capture gets also governs a shipped pack.
//!
//! ## Why this exists
//!
//! The 2026-10-02 retest (newt-agent #2660) measured a default-permissions
//! session prompting 129 times in one run, mostly for `git`, `find`, `sort`,
//! `head`, `grep`, `sed`, `awk`, and `xargs` — the ordinary cost of reading
//! text with no durable grant yet. A fresh install can propose and SIGN the
//! low-danger slice of that list up front, at `newt setup`, instead of
//! learning it one prompt at a time.
//!
//! ## What is deliberately excluded
//!
//! - **`git`/`gh`** are never candidates — enforced TWICE: [`STANDARD_TEXT_TOOLS`]
//!   never lists them, AND [`drop_never_candidates`] strips any basename match
//!   (including an absolute-path spelling like `/usr/bin/git`) out of the pool
//!   AFTER a drop-in's names are merged in, so a drop-in cannot reintroduce
//!   them. Git authority already goes through its own governed paths — the
//!   staging-repo push/PR broker (#2641, `crate::git_staging`, merged) and the
//!   worktree-commit ref grant (#2682, open) — neither of which is a plain
//!   exec-allowlist entry. A blanket `git` exec grant here would authorize
//!   every invocation, INCLUDING push, through a path neither broker gates.
//! - **`sed`/`find`** are not in the pool: their `e` command / `s///e` flag
//!   (sed) and `-exec`/`-delete`/`-ok` (find) run another program, and that
//!   descendant does not reliably re-enter Brush's exec interceptor
//!   (`vendor/agent-bridle-tool-shell/src/brush_shell.rs`). newt's native
//!   `grep` and `find` tools cover the same jobs without a spawn. The
//!   contract for everything that remains is **"can't run other programs;
//!   writes are still gated by `fs_write`"** — not "read-only" (`sed -i`
//!   wasn't offered, but the thing that made `sed` worth dropping is the
//!   execution hole, not the in-place edit).
//! - **Interpreters / command-runners** (`awk`, `xargs`, `sh`, …) — per the
//!   production danger table (`newt-tui::danger::INTERPRETER_EXEC`), granting
//!   bare execution of one of these is equivalent to an open-ended shell
//!   grant (`awk`'s `system()`; `xargs`/`sh -c`'s arbitrary child). They are
//!   NAMED in [`Proposal::deferred`] (via the shared danger gate inside
//!   [`propose_from_capture`]) with a reason, rather than silently vanishing
//!   from the list — but they are never signed: `PolicySet::validate_approve`
//!   unconditionally refuses a high-danger target, so there is no "opt in"
//!   that changes the outcome today. The standing path for these is the
//!   passkey step-up `danger.rs` already documents as unbuilt (P3).

use std::collections::HashSet;

use crate::flight_recorder::{FlightCapture, ShadowAxis};
use crate::ocap_propose::{propose_from_capture, Proposal};
use crate::ocap_store::CapabilityClass;

/// Provenance stamped on a pack-sourced entry's `by` field — the shipped
/// "common-settings profile" case `ExecEntry::by`'s doc comment names.
/// Distinct from [`crate::ocap_propose::PROVENANCE`] ("flight-recorder"):
/// these candidates were never actually observed in a real session.
pub const SETUP_PACK_PROVENANCE: &str = "seed";

/// The repro/fixture text stamped as the synthetic observation's `command` —
/// surfaces in the proposal's "learned from: …" note.
const SETUP_PACK_COMMAND: &str = "newt setup: standard caveat pack";

/// The standard pack's exec candidate pool — pure data (three-Cs): a POOL,
/// not a grant. Every name here still goes through the same danger gate
/// [`propose_standard_pack`] composes with.
///
/// Low-danger: POSIX-common text tools that **can't run other programs**
/// (the exec-allowlist grant is name-based, not argument-scoped — the bar
/// for inclusion is that no flag of the command spawns a child process; a
/// tool that can, like `sed`'s `e`/`s///e` or `find`'s `-exec`, is left out
/// rather than narrowed, since the grant can't see the flag): `cat`, `head`,
/// `tail`, `grep`, `sort`, `wc`, `ls`.
///
/// High-danger (interpreter / command-runner, per
/// `newt-tui::danger::INTERPRETER_EXEC`) — included here so the shared
/// danger gate NAMES them in [`Proposal::deferred`] with a reason, instead of
/// the pack silently never mentioning why they keep prompting: `awk`,
/// `xargs`, `sh`.
pub const STANDARD_TEXT_TOOLS: &[&str] = &[
    "cat", "head", "tail", "grep", "sort", "wc", "ls", "awk", "xargs", "sh",
];

/// Names whose exec grant must never enter the candidate pool, no matter what
/// a drop-in asks for — `git`/`gh` authority is mediated elsewhere (see the
/// module docs' "What is deliberately excluded"). Matched on the command's
/// file-name component (same convention as
/// `newt_tui::danger::DangerTable::is_interpreter`), so an absolute-path
/// spelling like `/usr/bin/git` is caught too.
const NEVER_CANDIDATES: &[&str] = &["git", "gh"];

/// Strip any [`NEVER_CANDIDATES`] basename out of a merged candidate pool.
/// Call this AFTER [`merge_candidates`] — the built-in pool never lists
/// `git`/`gh`, but a drop-in's `exec` list is operator/project data this
/// function must not trust to omit them.
pub fn drop_never_candidates<'a>(candidates: &[&'a str]) -> Vec<&'a str> {
    candidates
        .iter()
        .copied()
        .filter(|name| {
            let base = std::path::Path::new(name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(name);
            !NEVER_CANDIDATES.contains(&base)
        })
        .collect()
}

/// An operator/project drop-in that ADDS exec/net candidate names to the
/// built-in pool above — the same droppable-`.toml`-over-pure-data
/// convention `LanguagePack` ([`crate::api_surface`]) already establishes.
/// Ships at `<config dir>/caveat-pack.toml`. A duplicate of a built-in name
/// is folded out by [`merge_candidates`]; this widens the candidate POOL
/// only — every name, built-in or dropped-in, still goes through the same
/// danger gate and the same per-run operator review before anything is
/// signed.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct CaveatPackDropIn {
    #[serde(default)]
    pub exec: Vec<String>,
    #[serde(default)]
    pub net: Vec<String>,
}

impl CaveatPackDropIn {
    /// Parse a drop-in file's contents. A malformed file is the caller's to
    /// warn about and skip — never fatal to the setup step (matches
    /// `api_surface::load_packs_from_dir`'s rule for a bad language pack).
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("caveat-pack.toml: {e}"))
    }
}

/// Merge a drop-in's names onto the built-in pool: built-ins first, then
/// any new drop-in name, no duplicates. Order otherwise stable — it affects
/// display order only, never grant semantics (every candidate goes through
/// the same gate regardless of position).
pub fn merge_candidates<'a>(builtin: &[&'a str], extra: &'a [String]) -> Vec<&'a str> {
    let mut pool: Vec<&str> = Vec::with_capacity(builtin.len() + extra.len());
    pool.extend(builtin.iter().copied());
    for name in extra {
        let name = name.trim();
        if !name.is_empty() && !pool.contains(&name) {
            pool.push(name);
        }
    }
    pool
}

/// Fold the standard pack's candidate pool into a [`Proposal`] — the setup
/// step's pure core. `exec_candidates`/`net_candidates` is whatever
/// [`merge_candidates`] (or a bare slice) produced; `in_policy` is the
/// current store's accounted-for `(class, target)` pairs, read unverified
/// across every verdict — same convention
/// [`crate::ocap_propose::in_policy_pairs`] documents: an already-written
/// unsigned candidate must count as handled, so re-running setup converges.
/// `is_high_danger` is the production danger table's predicate
/// (`newt_tui::ocap_high_danger_predicate`), injected so this stays
/// fs/tty/wall-clock-free. `now` is the ISO date stamped on `granted`.
///
/// Reuses [`propose_from_capture`] wholesale (via the synthetic capture
/// described in the module docs) so the gap-dedup and the danger-defer
/// partition are the ONE engine a flight-recorder proposal and a shipped
/// pack both go through — then re-stamps `by` from `PROVENANCE`
/// ("flight-recorder", wrong here: nothing was observed) to
/// [`SETUP_PACK_PROVENANCE`] ("seed").
pub fn propose_standard_pack(
    exec_candidates: &[&str],
    net_candidates: &[&str],
    in_policy: &HashSet<(String, String)>,
    is_high_danger: impl Fn(CapabilityClass, &str) -> bool,
    now: &str,
) -> Proposal {
    let mut capture = FlightCapture::default();
    for &target in exec_candidates {
        capture.observe(ShadowAxis::Exec, target, SETUP_PACK_COMMAND, None);
    }
    for &host in net_candidates {
        capture.observe(ShadowAxis::Net, host, SETUP_PACK_COMMAND, None);
    }
    let mut proposal = propose_from_capture(&capture, in_policy, is_high_danger, now);
    for e in &mut proposal.additions.exec {
        e.by = Some(SETUP_PACK_PROVENANCE.to_string());
    }
    for e in &mut proposal.additions.net {
        e.by = Some(SETUP_PACK_PROVENANCE.to_string());
    }
    proposal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A danger predicate shaped like the production table
    /// (`newt-tui::danger::DangerTable`, unreachable from this lower layer)
    /// without duplicating it: interpreters/command-runners high, everything
    /// else low. `newt-tui/src/setup/tests.rs` is what proves the REAL table
    /// agrees on these targets.
    fn danger(class: CapabilityClass, target: &str) -> bool {
        matches!(class, CapabilityClass::Exec) && matches!(target, "awk" | "xargs" | "sh")
    }

    /// #2660: git authority is mediated by the governed push broker (#2641)
    /// and the worktree-commit grant (#2682), never a blanket exec-allowlist
    /// entry. Red if either ever sneaks back into the pool.
    #[test]
    fn standard_pack_never_lists_git_or_gh() {
        assert!(!STANDARD_TEXT_TOOLS.contains(&"git"));
        assert!(!STANDARD_TEXT_TOOLS.contains(&"gh"));
    }

    #[test]
    fn propose_standard_pack_signs_low_danger_and_defers_named_high_danger() {
        let p = propose_standard_pack(
            STANDARD_TEXT_TOOLS,
            &["127.0.0.1"],
            &HashSet::new(),
            danger,
            "2026-10-02",
        );
        let exec_targets: Vec<&str> = p.additions.exec.iter().map(|e| e.target.as_str()).collect();
        for low in ["cat", "head", "tail", "grep", "sort", "wc", "ls"] {
            assert!(
                exec_targets.contains(&low),
                "{low} missing from {exec_targets:?}"
            );
        }
        for e in &p.additions.exec {
            assert_eq!(e.by.as_deref(), Some(SETUP_PACK_PROVENANCE));
            assert!(
                e.sig.is_none(),
                "candidates are unsigned until the setup step blesses them"
            );
        }
        assert_eq!(p.additions.net.len(), 1);
        assert_eq!(p.additions.net[0].host, "127.0.0.1");
        assert_eq!(
            p.additions.net[0].by.as_deref(),
            Some(SETUP_PACK_PROVENANCE)
        );

        let deferred_targets: Vec<&str> = p.deferred.iter().map(|d| d.target.as_str()).collect();
        for high in ["awk", "xargs", "sh"] {
            assert!(
                deferred_targets.contains(&high),
                "{high} missing from {deferred_targets:?}"
            );
        }
        // High-danger targets are named in `deferred`, never signed into
        // `additions` — the "opt in" question is a no-op by construction.
        for low in ["cat", "head", "tail", "grep", "sort", "wc", "ls"] {
            assert!(!deferred_targets.contains(&low));
        }
    }

    /// #2660 round 2, item 2: `sed`/`find` are gone from the pool — their
    /// execution flags (`sed`'s `e`/`s///e`, `find`'s `-exec`/`-delete`/
    /// `-ok`) run another program whose descendant doesn't reliably re-enter
    /// Brush's exec interceptor, and newt now has native `grep`/`find`
    /// tools for the read-only job these covered.
    #[test]
    fn standard_pack_drops_sed_and_find() {
        assert!(!STANDARD_TEXT_TOOLS.contains(&"sed"));
        assert!(!STANDARD_TEXT_TOOLS.contains(&"find"));
    }

    /// Re-running setup must not re-propose what a prior run (or a hand
    /// edit) already accounted for — same idempotence
    /// `ocap_propose::propose_from_capture` already guarantees for a
    /// flight-recorder capture.
    #[test]
    fn propose_standard_pack_skips_targets_already_in_policy() {
        let mut in_policy = HashSet::new();
        in_policy.insert(("exec".to_string(), "cat".to_string()));
        let p = propose_standard_pack(&["cat", "head"], &[], &in_policy, danger, "2026-10-02");
        assert_eq!(p.additions.exec.len(), 1);
        assert_eq!(p.additions.exec[0].target, "head");
    }

    #[test]
    fn merge_candidates_dedupes_and_keeps_builtins_first() {
        let builtin = ["cat", "ls"];
        let extra = vec!["rg".to_string(), "cat".to_string(), " ".to_string()];
        let merged = merge_candidates(&builtin, &extra);
        assert_eq!(merged, vec!["cat", "ls", "rg"]);
    }

    /// #2660 round 2, item 3: a drop-in can ask for `git`/`gh` by name, or by
    /// an absolute-path spelling — `drop_never_candidates` must strip both
    /// AFTER the merge, since the built-in pool alone never covers drop-in
    /// input. Red before the fix: `merge_candidates` alone has no opinion on
    /// `git`, and `git` is Low-danger in the production table, so nothing
    /// downstream would have caught it.
    #[test]
    fn drop_never_candidates_strips_git_and_gh_by_basename() {
        let extra = [
            "git".to_string(),
            "/usr/bin/git".to_string(),
            "gh".to_string(),
            "rg".to_string(),
        ];
        let pool = merge_candidates(&["cat"], &extra);
        let filtered = drop_never_candidates(&pool);
        assert_eq!(filtered, vec!["cat", "rg"]);
    }

    #[test]
    fn drop_in_parses_additional_candidates() {
        let parsed =
            CaveatPackDropIn::parse("exec = [\"rg\"]\nnet = [\"example.internal\"]\n").unwrap();
        assert_eq!(parsed.exec, vec!["rg".to_string()]);
        assert_eq!(parsed.net, vec!["example.internal".to_string()]);
    }

    #[test]
    fn drop_in_malformed_is_a_reported_error_not_a_panic() {
        assert!(CaveatPackDropIn::parse("not valid toml {{{").is_err());
    }
}
