//! Flight-recorder → candidate policy (#1176) — fold a `--full-access`
//! session's observed authority into a **reviewable, unsigned** `approve.toml`
//! proposal. This is the accumulation loop *inverted*: instead of
//! prompt → grant, it is **observe → catalog → promote**.
//!
//! The [`crate::flight_recorder`] captures what authority an unconfined session
//! actually used (a [`FlightCapture`]). This module turns that capture into a
//! proposal an operator reviews and then blesses with `newt doctor --sign-ocap`
//! — the *only* path by which an observed caveat becomes a durable grant. No
//! step here signs anything or widens the floor; it proposes.
//!
//! Two hard rules, both inherited from the policy contract (agent-bridle
//! `policy`) and the flight-recorder's own law:
//! - **Never an auto-grant.** Only caveats *not already accounted for* by the
//!   current store ([`FlightCapture::gaps`]) are proposed, and every proposed
//!   entry is written UNSIGNED (`sig = None`) — it does nothing until a present
//!   human blesses it.
//! - **Danger-gated exactly as a real grant.** A high-danger target (per the
//!   caller's danger table, injected as a predicate) is NEVER proposed into
//!   `approve.toml` — `sign_approves`/`validate_approve` would refuse it and
//!   `verified_approves` would drop it at load anyway. It is reported as a
//!   [`DeferredCaveat`] ("keep prompting / offer passkey step-up") instead.
//!
//! Pure: given a folded capture, the (class, target) set the store already
//! covers, the danger predicate, and a timestamp, it returns a [`Proposal`].
//! The CLI (`newt ocap propose`) wires the fs read/write around it.

use std::collections::HashSet;

use crate::flight_recorder::{FlightCapture, ShadowAxis};
use crate::ocap_store::{CapabilityClass, ExecEntry, FsEntry, NetEntry, PolicyFile, PolicySet};

/// The provenance stamped on every proposed entry's `by` field — mirrors the
/// contract's free-form provenance (`human` / `seed` / a tool name). This value
/// tells a reviewer the entry came from an observed session, not a human
/// decision, so it earns extra scrutiny before blessing.
pub const PROVENANCE: &str = "flight-recorder";

/// An observed authority the proposal did NOT turn into a durable approve —
/// because the current danger table classifies it high-danger. Reported to the
/// operator so nothing observed is silently dropped: it stays a prompt, or the
/// operator can add it to `passkey.toml` for WebAuthn step-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredCaveat {
    /// The policy capability-class label: `exec` / `fs` / `net`.
    pub class: &'static str,
    /// The gated target (exec program, fs path, or net host).
    pub target: String,
    /// The raw command that produced the observation (the repro fixture).
    pub command: String,
    /// How many times it was observed this session.
    pub count: u64,
    /// Operator-facing reason it is not durably approvable.
    pub reason: String,
}

/// The output of a propose pass: the low-danger additions to fold into
/// `approve.toml` (unsigned candidates) plus the high-danger observations that
/// were deferred rather than proposed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Proposal {
    /// New, low-danger entries to append to `approve.toml`. Every entry is
    /// `sig = None`: a candidate awaiting `newt doctor --sign-ocap`.
    pub additions: PolicyFile,
    /// High-danger observations that were NOT proposed (see [`DeferredCaveat`]).
    pub deferred: Vec<DeferredCaveat>,
}

impl Proposal {
    /// The count of proposed additions across all classes.
    pub fn additions_len(&self) -> usize {
        self.additions.exec.len() + self.additions.fs.len() + self.additions.net.len()
    }

    /// True when there is nothing to propose AND nothing to defer — the capture
    /// is fully accounted for by the current store.
    pub fn is_empty(&self) -> bool {
        self.additions_len() == 0 && self.deferred.is_empty()
    }
}

/// The (class, target) pairs the current store already accounts for, across
/// EVERY verdict. A `deny`/`ask`/`passkey` entry answers a would-be gap just as
/// an `approve` does — the operator has already made a decision about it — so a
/// re-run does not re-propose it. Build this from the UNVERIFIED store (see the
/// CLI): an already-written-but-unsigned candidate must count as accounted-for,
/// or `propose` would re-offer it every run (the signature-verify load pass
/// would drop the unsigned candidate and never converge).
pub fn in_policy_pairs(set: &PolicySet) -> HashSet<(String, String)> {
    let mut pairs = HashSet::new();
    for file in set.files.values() {
        for e in &file.exec {
            pairs.insert(("exec".to_string(), e.target.clone()));
        }
        for e in &file.fs {
            pairs.insert(("fs".to_string(), e.path.clone()));
        }
        for e in &file.net {
            pairs.insert(("net".to_string(), e.host.clone()));
        }
    }
    pairs
}

/// The policy capability class a shadow axis maps to (the same mapping
/// [`crate::ocap_store::class_for`] uses for the denial axes).
fn axis_class(axis: ShadowAxis) -> CapabilityClass {
    match axis {
        ShadowAxis::Exec => CapabilityClass::Exec,
        ShadowAxis::FsRead | ShadowAxis::FsWrite => CapabilityClass::Fs,
        ShadowAxis::Net => CapabilityClass::Net,
    }
}

/// Why a high-danger observation cannot become a durable approve — operator
/// guidance, class-specific.
fn defer_reason(class: CapabilityClass) -> String {
    match class {
        CapabilityClass::Exec => "high-danger exec (interpreter / shell / command-runner) — \
             never durably approvable; keep prompting, or add to passkey.toml for WebAuthn step-up"
            .to_string(),
        CapabilityClass::Fs => "high-danger fs (a broad root at or above $HOME, the workspace, \
             or /) — never durably approvable; narrow the path, or use passkey.toml"
            .to_string(),
        // Net is Low by construction (a single host is narrow) — the danger
        // table never returns High for it, so this arm is unreachable in
        // practice; kept for exhaustiveness and honesty if the table changes.
        CapabilityClass::Net => {
            "high-danger net — never durably approvable; use passkey.toml".to_string()
        }
    }
}

/// Fold a capture into a [`Proposal`]: the gaps (observed − already-in-policy),
/// partitioned by danger. Low-danger gaps become unsigned `approve.toml`
/// candidates stamped with provenance and a repro note; high-danger gaps become
/// [`DeferredCaveat`]s. `now` is the ISO-8601 date to stamp on `granted`
/// (injected so the pure core stays wall-clock-free per the unit-test law).
pub fn propose_from_capture(
    capture: &FlightCapture,
    in_policy: &HashSet<(String, String)>,
    is_high_danger: impl Fn(CapabilityClass, &str) -> bool,
    now: &str,
) -> Proposal {
    let mut proposal = Proposal::default();
    for c in capture.gaps(in_policy) {
        let class = axis_class(c.axis);
        if is_high_danger(class, &c.target) {
            proposal.deferred.push(DeferredCaveat {
                class: c.axis.class(),
                target: c.target.clone(),
                command: c.command.clone(),
                count: c.count,
                reason: defer_reason(class),
            });
            continue;
        }
        let note = Some(format!("learned from: {} ({}x)", c.command, c.count));
        let granted = Some(now.to_string());
        let by = Some(PROVENANCE.to_string());
        match c.axis {
            ShadowAxis::Exec => proposal.additions.exec.push(ExecEntry {
                target: c.target.clone(),
                note,
                granted,
                by,
                sig: None,
            }),
            ShadowAxis::FsRead => proposal.additions.fs.push(FsEntry {
                path: c.target.clone(),
                write: false,
                note,
                granted,
                by,
                sig: None,
            }),
            ShadowAxis::FsWrite => proposal.additions.fs.push(FsEntry {
                path: c.target.clone(),
                write: true,
                note,
                granted,
                by,
                sig: None,
            }),
            ShadowAxis::Net => proposal.additions.net.push(NetEntry {
                host: c.target.clone(),
                note,
                granted,
                by,
                sig: None,
            }),
        }
    }
    proposal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flight_recorder::read_capture_jsonl;
    use crate::ocap_store::{build_store, Verdict};

    /// A danger predicate matching the production shape: interpreters/shells are
    /// high-danger exec; a `/`-rooted path is high-danger fs; net is always low.
    fn danger(class: CapabilityClass, target: &str) -> bool {
        match class {
            CapabilityClass::Exec => matches!(target, "bash" | "sh" | "python3" | "env"),
            CapabilityClass::Fs => target == "/" || target.starts_with("/home"),
            CapabilityClass::Net => false,
        }
    }

    fn capture(lines: &[&str]) -> FlightCapture {
        read_capture_jsonl(&lines.join("\n"))
    }

    #[test]
    fn low_danger_exec_gaps_become_unsigned_candidates() {
        // `cargo` is a fresh, low-danger observation → proposed unsigned, with
        // provenance + a repro note carrying the count.
        let cap =
            capture(&[r#"{"axis":"exec","target":"cargo","command":"cargo build","count":3}"#]);
        let p = propose_from_capture(&cap, &HashSet::new(), danger, "2026-07-17");
        assert_eq!(p.additions.exec.len(), 1);
        let e = &p.additions.exec[0];
        assert_eq!(e.target, "cargo");
        assert_eq!(e.by.as_deref(), Some("flight-recorder"));
        assert_eq!(e.granted.as_deref(), Some("2026-07-17"));
        assert!(e.sig.is_none(), "candidates are unsigned until blessed");
        assert_eq!(e.note.as_deref(), Some("learned from: cargo build (3x)"));
        assert!(p.deferred.is_empty());
    }

    #[test]
    fn high_danger_exec_is_deferred_never_proposed() {
        // The whole point of #1176's danger gate: an observed `bash` is NOT
        // laundered into approve.toml; it is reported for passkey/prompt.
        let cap = capture(&[
            r#"{"axis":"exec","target":"cargo","command":"cargo test","count":1}"#,
            r#"{"axis":"exec","target":"bash","command":"bash -c 'x'","count":2}"#,
        ]);
        let p = propose_from_capture(&cap, &HashSet::new(), danger, "2026-07-17");
        assert_eq!(p.additions.exec.len(), 1, "only cargo proposed");
        assert_eq!(p.additions.exec[0].target, "cargo");
        assert_eq!(p.deferred.len(), 1);
        assert_eq!(p.deferred[0].target, "bash");
        assert_eq!(p.deferred[0].class, "exec");
        assert_eq!(p.deferred[0].count, 2);
        assert!(p.deferred[0].reason.contains("passkey"));
    }

    #[test]
    fn fs_read_and_write_axes_map_to_the_write_flag() {
        let cap = capture(&[
            r#"{"axis":"fs_read","target":"/ws/src","command":"cat x","count":1}"#,
            r#"{"axis":"fs_write","target":"/ws/out","command":"tee y","count":1}"#,
            r#"{"axis":"fs_write","target":"/home/x","command":"rm z","count":1}"#,
        ]);
        let p = propose_from_capture(&cap, &HashSet::new(), danger, "2026-07-17");
        // /ws paths are low-danger; the /home write is deferred.
        assert_eq!(p.additions.fs.len(), 2);
        let read = p.additions.fs.iter().find(|e| e.path == "/ws/src").unwrap();
        assert!(!read.write, "fs_read → read-only");
        let write = p.additions.fs.iter().find(|e| e.path == "/ws/out").unwrap();
        assert!(write.write, "fs_write → write coverage");
        assert_eq!(p.deferred.len(), 1);
        assert_eq!(p.deferred[0].target, "/home/x");
        assert_eq!(p.deferred[0].class, "fs");
    }

    #[test]
    fn net_gaps_become_candidates() {
        let cap = capture(&[r#"{"axis":"net","target":"crates.io","command":"curl","count":1}"#]);
        let p = propose_from_capture(&cap, &HashSet::new(), danger, "2026-07-17");
        assert_eq!(p.additions.net.len(), 1);
        assert_eq!(p.additions.net[0].host, "crates.io");
        assert!(p.additions.net[0].sig.is_none());
    }

    #[test]
    fn gaps_already_in_any_verdict_are_not_reproposed() {
        // A caveat the store already accounts for — under ANY verdict — is not
        // re-proposed. `cargo` is already an (unsigned) approve; `rm` is a deny.
        let (set, _) = build_store(&[
            (
                Verdict::Approve,
                Some("[[exec]]\ntarget = \"cargo\"\n".to_string()),
            ),
            (
                Verdict::Deny,
                Some("[[exec]]\ntarget = \"rm\"\n".to_string()),
            ),
        ]);
        let in_policy = in_policy_pairs(&set);
        let cap = capture(&[
            r#"{"axis":"exec","target":"cargo","command":"cargo build","count":1}"#,
            r#"{"axis":"exec","target":"rm","command":"rm -rf t","count":1}"#,
            r#"{"axis":"exec","target":"git","command":"git push","count":1}"#,
        ]);
        let p = propose_from_capture(&cap, &in_policy, danger, "2026-07-17");
        assert_eq!(p.additions.exec.len(), 1, "only the genuinely-new git");
        assert_eq!(p.additions.exec[0].target, "git");
        assert!(p.deferred.is_empty());
    }

    #[test]
    fn empty_capture_yields_an_empty_proposal() {
        let p = propose_from_capture(
            &FlightCapture::default(),
            &HashSet::new(),
            danger,
            "2026-07-17",
        );
        assert!(p.is_empty());
        assert_eq!(p.additions_len(), 0);
    }

    #[test]
    fn in_policy_pairs_walks_all_classes_and_verdicts() {
        let (set, _) = build_store(&[
            (
                Verdict::Approve,
                Some("[[exec]]\ntarget = \"cargo\"\n[[net]]\nhost = \"crates.io\"\n".to_string()),
            ),
            (
                Verdict::Ask,
                Some("[[fs]]\npath = \"/ws\"\nwrite = true\n".to_string()),
            ),
        ]);
        let pairs = in_policy_pairs(&set);
        assert!(pairs.contains(&("exec".to_string(), "cargo".to_string())));
        assert!(pairs.contains(&("net".to_string(), "crates.io".to_string())));
        assert!(pairs.contains(&("fs".to_string(), "/ws".to_string())));
        assert_eq!(pairs.len(), 3);
    }
}
