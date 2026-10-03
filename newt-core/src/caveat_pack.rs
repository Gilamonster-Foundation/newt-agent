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
//! - **A drop-in can only NARROW the built-in exec pool, never widen it**
//!   (#2660 round 3, item 2). [`CaveatPackDropIn::exec`] is read as a list of
//!   built-in names to drop, by [`narrow_candidates`] — any entry that is not
//!   already in [`STANDARD_TEXT_TOOLS`] is an attempted ADDITION, reported
//!   back (never merged in). This closes the exact bypass the round-2 review
//!   found: with the old merge-then-filter shape, a drop-in listing
//!   `sed`/`find` (or any other command-runner) reintroduced them under the
//!   same "can't run other programs" promise the built-in pool review
//!   covers, and only `git`/`gh` were ever filtered back out. Offering an
//!   operator-chosen program is a separate, explicitly-labelled feature with
//!   its own confirmation flow — out of scope here.
//! - **`git`/`gh`** are never candidates — [`STANDARD_TEXT_TOOLS`] never lists
//!   them, and (now unreachable via a drop-in, since narrowing can't
//!   introduce a name) [`drop_never_candidates`] still strips any basename
//!   match — including an absolute-path spelling like `/usr/bin/git` or a
//!   Windows spelling (`git.exe`, any case) — as a defense-in-depth backstop
//!   against the built-in list itself ever growing a `git`/`gh` entry by
//!   mistake. Git authority already goes through its own governed paths —
//!   the staging-repo push/PR broker (#2641, `crate::git_staging`, merged)
//!   and the worktree-commit ref grant (#2682, open) — neither of which is a
//!   plain exec-allowlist entry. A blanket `git` exec grant here would
//!   authorize every invocation, INCLUDING push, through a path neither
//!   broker gates.
//! - **`sed`/`find`/`sort`** are not in the pool: their `e` command / `s///e`
//!   flag (sed), `-exec`/`-delete`/`-ok` (find), and `--compress-program=PROG`
//!   (sort — #2660 round 3, item 1) each run another program, and that
//!   descendant does not reliably re-enter Brush's exec interceptor
//!   (`vendor/agent-bridle-tool-shell/src/brush_shell.rs`). The production
//!   danger table has no opinion on any of these three — they are ordinary
//!   low-danger text tools to it — so the ONLY thing keeping them from being
//!   signed is never offering them in the first place. newt's native `grep`
//!   and `find` tools cover the jobs `sed`/`find` did without a spawn. The
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
/// [`propose_standard_pack`] composes with. [`narrow_candidates`] is the
/// ONLY way a drop-in may touch this list (remove an entry); nothing may be
/// added to it outside this source file (#2660 round 3, item 2).
///
/// Low-danger: POSIX-common text tools **REVIEWED against their own manual
/// for a flag that spawns another program, and confirmed clean** (the
/// exec-allowlist grant is name-based, not argument-scoped, so the bar for
/// inclusion is that NO flag of the command spawns a child process — a tool
/// that has one, like `sed`'s `e`/`s///e`, `find`'s `-exec`, or `sort`'s
/// `--compress-program`, is left out entirely rather than narrowed, since the
/// grant can't see the flag):
/// - `cat`  — no flag spawns a process (GNU/BSD/POSIX).
/// - `head`/`tail` — no flag spawns a process; `tail -f` only polls the file,
///   it never execs (GNU/BSD/POSIX).
/// - `grep` — no flag spawns a process; `--include`/`--exclude` filter paths
///   by glob, they don't invoke one (GNU/BSD/POSIX).
/// - `wc`   — no flag spawns a process (GNU/BSD/POSIX).
/// - `ls`   — no flag spawns a process; `--color` only reads env/terminfo
///   (GNU/BSD/POSIX).
///
/// High-danger (interpreter / command-runner, per
/// `newt-tui::danger::INTERPRETER_EXEC`) — included here so the shared
/// danger gate NAMES them in [`Proposal::deferred`] with a reason, instead of
/// the pack silently never mentioning why they keep prompting: `awk`,
/// `xargs`, `sh`.
pub const STANDARD_TEXT_TOOLS: &[&str] = &[
    "cat", "head", "tail", "grep", "wc", "ls", "awk", "xargs", "sh",
];

/// Names whose exec grant must never enter the candidate pool, no matter what
/// a drop-in asks for — `git`/`gh` authority is mediated elsewhere (see the
/// module docs' "What is deliberately excluded"). Matched on the command's
/// file-name component, normalized the way an executable's spelling varies
/// across platforms (same convention as
/// `newt_tui::danger::DangerTable::is_interpreter`, plus the normalization):
/// lowercased, and a trailing `.exe` stripped, so `/usr/bin/git`, `git.exe`,
/// `GIT.EXE`, and `C:/Program Files/Git/cmd/git.exe` are all caught (#2660
/// round 3, item 3 — Windows resolves an executable case-insensitively and by
/// that extension, which the bare POSIX spelling never needed).
///
/// With [`narrow_candidates`] in place, a drop-in can no longer introduce a
/// name that isn't already in [`STANDARD_TEXT_TOOLS`] at all (see the module
/// docs), so this filter is now defense-in-depth against the built-in list
/// itself ever growing a `git`/`gh` entry by mistake — not the primary gate.
const NEVER_CANDIDATES: &[&str] = &["git", "gh"];

/// Normalize a candidate's possibly-platform-specific executable spelling to
/// the bare lowercase basename [`NEVER_CANDIDATES`] matches against: strip a
/// directory component (so `/usr/bin/git` matches `git`) and a trailing
/// `.exe` (so `git.exe`/`GIT.EXE` match `git` too).
fn never_candidate_key(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    base.strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(base)
}

/// Strip any [`NEVER_CANDIDATES`] basename out of a candidate pool — kept as
/// a defense-in-depth backstop (#2660 round 3, item 3): since
/// [`narrow_candidates`] can only ever return a subset of the built-in pool,
/// and that pool never lists `git`/`gh`, a drop-in cannot reach this path
/// today. It still guards against the built-in list itself ever growing a
/// `git`/`gh` entry.
pub fn drop_never_candidates<'a>(candidates: &[&'a str]) -> Vec<&'a str> {
    candidates
        .iter()
        .copied()
        .filter(|name| !NEVER_CANDIDATES.contains(&never_candidate_key(name).as_str()))
        .collect()
}

/// An operator/project drop-in — the same droppable-`.toml`-over-pure-data
/// convention `LanguagePack` ([`crate::api_surface`]) already establishes.
/// Ships at `<config dir>/caveat-pack.toml`.
///
/// `exec` NARROWS the built-in exec pool ONLY (#2660 round 3, item 2, via
/// [`narrow_candidates`]): a listed name already in [`STANDARD_TEXT_TOOLS`]
/// is dropped from what gets offered; a listed name that ISN'T already
/// there is an attempted ADDITION and is reported back, never merged in.
/// The "can't run other programs" promise is reviewed once, against the
/// fixed built-in set — a drop-in that could widen it would let an
/// unreviewed program reach a signature under that same promise, which is
/// exactly how the round-2 fix (dropping `sed`/`find`) could have been
/// undone by a drop-in alone. Offering an operator-chosen program is a
/// separate, explicitly-labelled feature with its own confirmation flow —
/// out of scope here.
///
/// `net` still WIDENS the net candidate pool via [`merge_candidates`] — a
/// host is a network target, not a program, so the exec pool's "can't run
/// other programs" promise doesn't apply to it.
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

/// Merge a drop-in's names onto a built-in pool: built-ins first, then any
/// new drop-in name, no duplicates. Order otherwise stable — it affects
/// display order only, never grant semantics (every candidate goes through
/// the same gate regardless of position). Used for the NET pool only
/// (#2660 round 3, item 2) — the exec pool uses [`narrow_candidates`]
/// instead, since a drop-in may not widen it.
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

/// Narrow the built-in exec pool per a drop-in's requested removals — the
/// ONLY operation a drop-in may perform on it (#2660 round 3, item 2; see
/// [`CaveatPackDropIn::exec`]'s doc for why). Returns `(kept, ignored)`:
/// `kept` is `builtin` with every requested name that's actually IN it
/// removed; `ignored` is whatever was requested but matched nothing in
/// `builtin` — an attempted addition, handed back so the caller can report
/// it rather than let it silently do nothing. Blank entries are dropped
/// without being reported (same whitespace tolerance [`merge_candidates`]
/// has).
pub fn narrow_candidates<'a>(
    builtin: &[&'a str],
    requested_removals: &[String],
) -> (Vec<&'a str>, Vec<String>) {
    let requested: Vec<&str> = requested_removals
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let ignored = requested
        .iter()
        .filter(|name| !builtin.contains(name))
        .map(|name| name.to_string())
        .collect();
    let kept = builtin
        .iter()
        .copied()
        .filter(|name| !requested.contains(name))
        .collect();
    (kept, ignored)
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
        for low in ["cat", "head", "tail", "grep", "wc", "ls"] {
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
        for low in ["cat", "head", "tail", "grep", "wc", "ls"] {
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

    /// #2660 round 3, item 1 (P1), red-first: the production danger table has
    /// no opinion on `sort` (it's an ordinary low-danger text tool to it), but
    /// GNU/BSD `sort --compress-program=PROG` runs an arbitrary helper for
    /// temporary compression — a name-wide exec grant can't restrict that
    /// flag. Red before the fix: `sort` was still in [`STANDARD_TEXT_TOOLS`].
    #[test]
    fn standard_pack_drops_sort() {
        assert!(!STANDARD_TEXT_TOOLS.contains(&"sort"));
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

    /// #2660 round 3, item 2: this function is now used only for the NET
    /// pool — the exec pool goes through [`narrow_candidates`] instead,
    /// which cannot widen it. The merge/dedupe behavior itself is unchanged.
    #[test]
    fn merge_candidates_dedupes_and_keeps_builtins_first() {
        let builtin = ["cat", "ls"];
        let extra = vec!["rg".to_string(), "cat".to_string(), " ".to_string()];
        let merged = merge_candidates(&builtin, &extra);
        assert_eq!(merged, vec!["cat", "ls", "rg"]);
    }

    /// #2660 round 2, item 3: a drop-in can ask for `git`/`gh` by name, or by
    /// an absolute-path spelling — `drop_never_candidates` must strip both.
    /// Still exercised directly here (rather than through `merge_candidates`,
    /// which no longer feeds the exec pool) as the defense-in-depth path the
    /// module docs describe.
    #[test]
    fn drop_never_candidates_strips_git_and_gh_by_basename() {
        let pool = ["cat", "git", "/usr/bin/git", "gh", "rg"];
        let filtered = drop_never_candidates(&pool);
        assert_eq!(filtered, vec!["cat", "rg"]);
    }

    /// #2660 round 3, item 3 (P2), red-first: Windows resolves an executable
    /// case-insensitively and by a `.exe` suffix the POSIX spelling never
    /// needed — `git.exe`, `GIT.EXE`, and an absolute Windows-style path all
    /// have to match the same exclusion the bare `git` spelling already
    /// catches. Red before the fix: the old basename-only, case-sensitive
    /// compare let every one of these through (`"git.exe" != "git"`).
    #[test]
    fn drop_never_candidates_normalizes_windows_executable_spellings() {
        let pool = [
            "cat",
            "git.exe",
            "GIT.EXE",
            "C:/Program Files/Git/cmd/git.exe",
            "gh.exe",
            "Gh.Exe",
            "rg",
        ];
        let filtered = drop_never_candidates(&pool);
        assert_eq!(filtered, vec!["cat", "rg"]);
    }

    /// #2660 round 3, item 2 (P1), red-first: a requested removal that IS in
    /// the built-in pool is dropped; one that is NOT is an attempted
    /// addition, reported back rather than silently accepted. Red before the
    /// fix (when exec went through `merge_candidates`): `rg` would have been
    /// silently ADDED to the pool instead of reported as ignored.
    #[test]
    fn narrow_candidates_drops_known_entries_and_reports_unknown_ones() {
        let builtin = ["cat", "head", "tail"];
        let requested = vec!["head".to_string(), "rg".to_string(), " ".to_string()];
        let (kept, ignored) = narrow_candidates(&builtin, &requested);
        assert_eq!(kept, vec!["cat", "tail"]);
        assert_eq!(ignored, vec!["rg".to_string()]);
    }

    /// #2660 round 3, item 2: the exact scenario the review named — a
    /// drop-in asking to ADD `sed`/`find`/`sort`/`git` must come back as four
    /// ignored names, and the built-in pool must be untouched (none of the
    /// four was ever in it to begin with).
    #[test]
    fn narrow_candidates_ignores_a_drop_in_addition_attempt() {
        let requested = vec![
            "sed".to_string(),
            "find".to_string(),
            "sort".to_string(),
            "git".to_string(),
        ];
        let (kept, ignored) = narrow_candidates(STANDARD_TEXT_TOOLS, &requested);
        assert_eq!(kept, STANDARD_TEXT_TOOLS);
        assert_eq!(ignored, vec!["sed", "find", "sort", "git"]);
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
