//! Prompted ocap grants (issue #263) — the seam between a capability denial
//! and an interactive human decision.
//!
//! Default behavior is UNCHANGED: without a [`PermissionGate`] every denial
//! fails the tool call exactly as before. The TUI may supply a gate (the
//! `--prompt-for-permissions` flag / `[tui.permissions] prompt = true`);
//! headless callers (ACP worker, `newt-eval`) never construct one, so a
//! denial there can never block on a prompt — the eval gate stays honest.
//!
//! Ocap honesty: **attenuation-only is the invariant — a live key is never
//! widened.** An *allow* decision is implemented gate-side by re-minting a
//! fresh operating authority from the user root as (previous caveats ∪ new
//! grant); [`widen_caveats`] only builds that *policy* value — the signed
//! re-mint itself lives with the key holder (the TUI). The widened caveats
//! returned in [`PermissionDecision::Allow`] apply to the single re-executed
//! call; "allow for this session" is the gate remembering the grant so the
//! next denial of the same target re-mints without re-prompting. The
//! session's enforced baseline (`ChatCtx::caveats`) is never mutated.
//!
//! Every prompted decision is recorded ([`PermissionRecord`]) with the
//! conversation id so implicit denials can be promoted to explicit config
//! grants — or stay denied — deliberately. The record is a REVIEW artifact,
//! not config: nothing reads it back into authority. Promotion to a durable
//! grant is a human editing `[tui.permissions]`.

use crate::caveats::{Caveats, Scope};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::Path;

/// The capability axis a denial occurred on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DenialKind {
    /// `exec` — a command outside the granted exec allowlist.
    Exec,
    /// `fs_read` — a path outside the granted read scope.
    FsRead,
    /// `fs_write` — a path outside the granted write scope.
    FsWrite,
    /// `net` — a host outside the granted net allowlist.
    Net,
    /// FR-2 (#1001): a remote MCP `server__tool` the active persona does not
    /// grant. Unlike the four axes above it maps to NO fs/exec/net caveat — a
    /// remote tool's leash is name-based — so an `Allow` here is purely "proceed
    /// with this call" (it widens nothing). `target` is the tool name.
    RemoteTool,
    /// #1056: a LOCAL git write (`add`/`commit`/`reset`/`branch`/…) the embedded
    /// git tool's projected [`GitCaveats`](crate::git_caveats::GitCaveats) does
    /// not grant. Like [`RemoteTool`](Self::RemoteTool) it maps to NO
    /// fs/exec/net caveat — the git tool's leash is its own op-class lattice, not
    /// the shell allowlist — so an `Allow` means "proceed with local git writes
    /// this call" (it widens no `Caveats` axis; the git arm re-dispatches under a
    /// local-write `GitCaveats`). `target` is the git op (`commit`, `add`, …).
    /// The network verbs (push/fetch/clone) are NOT this capability — they stay
    /// deferred / shell-net-gated. A readonly `/mode` still denies it (the gate
    /// refuses the grant when the preset floor projects no git write).
    GitWrite,
}

impl DenialKind {
    /// Stable string form — also the `kind` field of [`PermissionRecord`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::FsRead => "fs_read",
            Self::FsWrite => "fs_write",
            Self::Net => "net",
            Self::RemoteTool => "remote_tool",
            Self::GitWrite => "git_write",
        }
    }
}

/// Inverse of [`DenialKind::as_str`] — parse a persisted `kind`. `Err(())` for
/// an unknown string so a corrupt/forward-incompatible denylist line is skipped
/// rather than trusted (#904). A trait impl (not an inherent `from_str`) so
/// `"net".parse::<DenialKind>()` works and clippy's `should_implement_trait` is
/// satisfied.
impl std::str::FromStr for DenialKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "exec" => Ok(Self::Exec),
            "fs_read" => Ok(Self::FsRead),
            "fs_write" => Ok(Self::FsWrite),
            "net" => Ok(Self::Net),
            "remote_tool" => Ok(Self::RemoteTool),
            "git_write" => Ok(Self::GitWrite),
            _ => Err(()),
        }
    }
}

/// #904: one durable "permanently deny" entry — a `(kind, target)` the human
/// chose to deny across restarts, so the gate refuses it WITHOUT re-prompting.
/// Stored one JSON line per entry in `~/.newt/permission-denials.jsonl` (a
/// sibling of `permission-log.jsonl`).
///
/// Unlike the permission LOG (a review artifact that is never read back into
/// authority), this file IS read back — at gate construction — and consulted
/// before prompting. That is sound precisely because it is **deny-only**: a
/// denylist can never widen authority, so reading it back cannot break the
/// attenuation-only invariant. A permanent *allow* has no analogue here; a
/// durable grant stays a deliberate `[tui.permissions]` edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentDenial {
    /// Capability axis: `exec` / `fs_read` / `fs_write` / `net`.
    pub kind: String,
    /// What is denied — a command name (exec), a path (fs_*), or a host (net).
    pub target: String,
    /// Wall-clock at decision time (RFC 3339, UTC) — a display claim, never
    /// an ordering key.
    pub ts_claim: String,
}

/// Parse a denials file body into `(kind, target)` pairs — PURE (no I/O), so it
/// unit-tests without a filesystem. Malformed lines and unknown `kind`s are
/// skipped (a corrupt entry must never crash the gate or, worse, be trusted).
pub fn parse_denials(contents: &str) -> Vec<(DenialKind, String)> {
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let d: PersistentDenial = serde_json::from_str(line).ok()?;
            let kind: DenialKind = d.kind.parse().ok()?;
            (!d.target.is_empty()).then_some((kind, d.target))
        })
        .collect()
}

/// Load the persistent denylist from `path`. A missing file is an empty list
/// (not an error) — the common first-run case.
pub fn load_denials(path: &Path) -> Vec<(DenialKind, String)> {
    match std::fs::read_to_string(path) {
        Ok(body) => parse_denials(&body),
        Err(_) => Vec::new(),
    }
}

/// Append one permanent-deny entry as a JSON line, creating parent dirs as
/// needed. Mirrors [`PermissionRecord::append_jsonl`].
pub fn append_denial(path: &Path, kind: DenialKind, target: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entry = PersistentDenial {
        kind: kind.as_str().to_string(),
        target: target.to_string(),
        ts_claim: chrono::Utc::now().to_rfc3339(),
    };
    let line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// One denied capability surfaced for a human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// The tool the model called (`run_command`, `read_file`, …).
    pub tool: String,
    /// Capability axis the denial occurred on.
    pub kind: DenialKind,
    /// What an *allow* would grant: a command name (exec), an absolute
    /// path (fs_read / fs_write), or a host (net).
    pub target: String,
    /// The denial text the model would otherwise see — shown to the human
    /// for context.
    pub reason: String,
}

/// Verdict from consulting the gate.
pub enum PermissionDecision {
    /// Re-execute the denied call under these caveats. They are a freshly
    /// minted authority (root ∪ grant) — the session's live key/baseline is
    /// untouched, and the value does not outlive the re-executed call.
    Allow(Caveats),
    /// Keep the standard structured denial, bit-for-bit.
    Deny,
}

/// The interactive human-interface seam (mirrors `NoteSink` / `RecallSource`):
/// the one gate the agentic loop consults whenever it must reach the human. It
/// carries two distinct interactions that share the same operator presence:
///
/// * [`PermissionGate::ask`] — decide a capability GRANT (the #263 / #721 ocap
///   flow). It can WIDEN authority by re-minting caveats.
/// * [`PermissionGate::ask_question`] — ask a free-text QUESTION and read back
///   the answer (the #728 `request_user_input` tool). It only gathers text; it
///   never touches authority.
///
/// Keeping both on one gate realizes "both surface to the human" without merging
/// the tools: grants flow through `ask`, questions through `ask_question`.
///
/// The call blocks the agentic loop like a long tool call (issue #263 §6 of
/// the design notes). Implementations that auto-allow previously
/// session-granted targets must still return freshly minted caveats.
pub trait PermissionGate {
    /// Ask about a batch of denials from ONE tool call (a compound command
    /// can be refused on several targets at once). `Allow` means every
    /// request was allowed; any single deny keeps the whole denial.
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision;

    /// #728: ask the human a free-text `question` and return their typed
    /// answer — the GENERIC ask-the-human primitive behind the
    /// `request_user_input` tool. Distinct from [`PermissionGate::ask`], which
    /// decides capability grants: `ask` can widen authority, `ask_question`
    /// only gathers text. Returns `None` when there is no human to consult
    /// (no interactive operator this session, or stdin closed) so the caller
    /// degrades to a recoverable "no human available" result instead of
    /// blocking — a headless caller must NEVER hang on it.
    fn ask_question(&mut self, question: &str) -> Option<String>;
}

/// Build the widened *policy* for a re-mint: `base` with each grant's target
/// added to its axis. `Scope::All` axes are left untouched (nothing to add);
/// this never narrows and never touches `max_calls` /
/// `valid_for_generation`. This is a plain policy value — minting it into a
/// signed key (and thereby proving base ⊑ root still holds) is the gate
/// implementation's job.
pub fn widen_caveats(base: &Caveats, grants: &[(DenialKind, String)]) -> Caveats {
    let mut out = base.clone();
    for (kind, target) in grants {
        let scope = match kind {
            DenialKind::Exec => &mut out.exec,
            DenialKind::FsRead => &mut out.fs_read,
            DenialKind::FsWrite => &mut out.fs_write,
            DenialKind::Net => &mut out.net,
            // FR-2 (#1001): a remote-tool grant maps to no caveat axis — the
            // `Allow` means "proceed with this call", not "widen an axis". Skip.
            DenialKind::RemoteTool => continue,
            // #1056: a git-write grant maps to no caveat axis either — the git
            // tool's authority is projected separately (GitCaveats); the git arm
            // re-dispatches under a local-write surface on Allow. Widen nothing.
            DenialKind::GitWrite => continue,
        };
        if let Scope::Only(set) = scope {
            set.insert(target.clone());
        }
    }
    out
}

/// One prompted permission decision, recorded with the session for later
/// review (issue #263). Serialized as one JSON line of
/// `~/.newt/permission-log.jsonl` until the Phase 17 store grows a
/// first-class events home for it.
///
/// This file is a record, NOT config: nothing reads it back into authority.
/// Promoting an `allow` to a durable grant is a human editing
/// `[tui.permissions]` (`extra_exec` / `net` / preset) — see issue #181.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecord {
    /// Wall-clock at decision time (RFC 3339, UTC). A display claim (§6) —
    /// never an ordering key; ordering is the file's append order.
    pub ts_claim: String,
    /// The conversation/session the decision belongs to.
    pub conversation_id: String,
    /// Tool whose call was denied.
    pub tool: String,
    /// Capability axis: `exec` / `fs_read` / `fs_write` / `net`.
    pub kind: String,
    /// What the decision was about (command name / path / host).
    pub target: String,
    /// `allow` or `deny`.
    pub decision: String,
    /// `once` (this call only) or `session` (until the session ends).
    pub scope: String,
}

impl PermissionRecord {
    /// Record one decision now. `ts_claim` is stamped from the wall clock —
    /// a display claim per the §6 discipline.
    pub fn new(
        conversation_id: &str,
        tool: &str,
        kind: DenialKind,
        target: &str,
        decision: &str,
        scope: &str,
    ) -> Self {
        Self {
            ts_claim: chrono::Utc::now().to_rfc3339(),
            conversation_id: conversation_id.to_string(),
            tool: tool.to_string(),
            kind: kind.as_str().to_string(),
            target: target.to_string(),
            decision: decision.to_string(),
            scope: scope.to_string(),
        }
    }

    /// Append this record as one JSON line, creating parent dirs as needed.
    pub fn append_jsonl(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caveats::{CaveatsExt as _, CountBound};

    fn base() -> Caveats {
        Caveats {
            fs_read: Scope::only(["/ws".to_string()]),
            fs_write: Scope::only(["/ws".to_string()]),
            exec: Scope::only(["cargo".to_string()]),
            net: Scope::none(),
            max_calls: CountBound::AtMost(7),
            valid_for_generation: Scope::All,
        }
    }

    #[test]
    fn denial_kind_strings_are_stable() {
        // Load-bearing: these are the `kind` values in the on-disk record
        // and the keys a human greps the log for.
        assert_eq!(DenialKind::Exec.as_str(), "exec");
        assert_eq!(DenialKind::FsRead.as_str(), "fs_read");
        assert_eq!(DenialKind::FsWrite.as_str(), "fs_write");
        assert_eq!(DenialKind::Net.as_str(), "net");
    }

    #[test]
    fn widen_adds_each_grant_to_its_axis_only() {
        let widened = widen_caveats(
            &base(),
            &[
                (DenialKind::Exec, "npm".to_string()),
                (DenialKind::Net, "docs.rs".to_string()),
                (DenialKind::FsRead, "/etc/hosts".to_string()),
                (DenialKind::FsWrite, "/tmp/out".to_string()),
            ],
        );
        assert!(widened.permits_exec("npm"));
        assert!(widened.permits_exec("cargo"), "existing grants kept");
        assert!(widened.permits_net("docs.rs"));
        assert!(widened.permits_fs_read("/etc/hosts"));
        assert!(widened.permits_fs_write("/tmp/out"));
        // Untouched axes / non-granted targets stay denied.
        assert!(!widened.permits_exec("rm"));
        assert!(!widened.permits_net("evil.example.com"));
        // Non-scope axes are never altered by a grant.
        assert_eq!(widened.max_calls, CountBound::AtMost(7));
    }

    #[test]
    fn widen_leaves_base_unchanged_and_all_stays_all() {
        let b = base();
        let _ = widen_caveats(&b, &[(DenialKind::Exec, "npm".to_string())]);
        assert_eq!(b, base(), "widen builds a NEW policy; base is immutable");

        let all = Caveats::top();
        let widened = widen_caveats(&all, &[(DenialKind::Exec, "npm".to_string())]);
        assert_eq!(widened, all, "Scope::All has nothing to add");
    }

    #[test]
    fn widen_with_no_grants_is_identity() {
        assert_eq!(widen_caveats(&base(), &[]), base());
    }

    #[test]
    fn record_serializes_the_issue_shape() {
        let rec = PermissionRecord::new(
            "conv-1",
            "run_command",
            DenialKind::Exec,
            "npm",
            "allow",
            "session",
        );
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(json["conversation_id"], "conv-1");
        assert_eq!(json["tool"], "run_command");
        assert_eq!(json["kind"], "exec");
        assert_eq!(json["target"], "npm");
        assert_eq!(json["decision"], "allow");
        assert_eq!(json["scope"], "session");
        // ts_claim is present and RFC 3339-shaped (a display claim, §6).
        let ts = json["ts_claim"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "got: {ts}"
        );
    }

    #[test]
    fn append_jsonl_appends_one_line_per_record_and_creates_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("permission-log.jsonl");
        let a = PermissionRecord::new(
            "conv-1",
            "read_file",
            DenialKind::FsRead,
            "/etc/hosts",
            "deny",
            "once",
        );
        let b = PermissionRecord::new(
            "conv-1",
            "web_fetch",
            DenialKind::Net,
            "docs.rs",
            "allow",
            "once",
        );
        a.append_jsonl(&path).unwrap();
        b.append_jsonl(&path).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: PermissionRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed, a, "round-trips losslessly");
        let parsed: PermissionRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed.kind, "net");
    }

    // ---- #904: persistent "permanently deny" store ----

    #[test]
    fn denial_kind_from_str_round_trips_and_rejects_unknown() {
        for k in [
            DenialKind::Exec,
            DenialKind::FsRead,
            DenialKind::FsWrite,
            DenialKind::Net,
        ] {
            assert_eq!(k.as_str().parse::<DenialKind>(), Ok(k));
        }
        assert!("nope".parse::<DenialKind>().is_err());
        assert!("".parse::<DenialKind>().is_err());
    }

    #[test]
    fn parse_denials_is_pure_and_skips_bad_lines() {
        let body = "\
{\"kind\":\"net\",\"target\":\"github.com\",\"ts_claim\":\"2026-07-04T00:00:00Z\"}
{\"kind\":\"exec\",\"target\":\"rm\",\"ts_claim\":\"2026-07-04T00:00:00Z\"}

not json at all
{\"kind\":\"bogus\",\"target\":\"x\",\"ts_claim\":\"2026-07-04T00:00:00Z\"}
{\"kind\":\"net\",\"target\":\"\",\"ts_claim\":\"2026-07-04T00:00:00Z\"}
";
        let got = parse_denials(body);
        // Only the two well-formed, known-kind, non-empty-target lines survive.
        assert_eq!(
            got,
            vec![
                (DenialKind::Net, "github.com".to_string()),
                (DenialKind::Exec, "rm".to_string()),
            ]
        );
    }

    #[test]
    fn load_denials_missing_file_is_empty_not_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nope").join("permission-denials.jsonl");
        assert!(load_denials(&missing).is_empty());
    }

    #[test]
    fn append_denial_then_load_round_trips_and_creates_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("permission-denials.jsonl");
        append_denial(&path, DenialKind::Net, "github.com").unwrap();
        append_denial(&path, DenialKind::Exec, "curl").unwrap();
        let loaded = load_denials(&path);
        assert_eq!(
            loaded,
            vec![
                (DenialKind::Net, "github.com".to_string()),
                (DenialKind::Exec, "curl".to_string()),
            ]
        );
    }
}
