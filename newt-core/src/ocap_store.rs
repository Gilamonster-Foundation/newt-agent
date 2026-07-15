//! The `~/.newt/ocap/` durable-policy store (#1131, epic #1126 track O2) — the
//! newt-side store that implements the agent-bridle policy contract (bridle
//! #221 / #220).
//!
//! The accumulation loop's memory: prompt → decision → **stored policy** →
//! fewer prompts. Four per-verdict TOML files under `~/.newt/ocap/` —
//! `approve.toml` / `deny.toml` / `ask.toml` / `passkey.toml` — each a bridle
//! [`PolicyFile`] of per-class entries (exec / fs / net). This module LOADS
//! them into a [`PolicySet`] and answers the one question the permission gate
//! asks: *"is there a durable verdict for this capability + target?"*
//!
//! Two rules from the contract (bridle `policy`), enforced by construction:
//! - **Precedence:** deny > passkey > ask > approve — the most restrictive
//!   durable verdict wins. [`PolicySet::evaluate`] applies it.
//! - **No match ⇒ fall through:** [`evaluate_request`] returns `None`, and the
//!   gate uses its interactive prompt / default-deny floor. Durable policy only
//!   ever *narrows or pre-answers*; it never widens the floor.
//!
//! Split for the mocked-unit tier (no real fs in unit tests): [`build_store`]
//! is pure (takes file contents); [`load_store`] is the thin fs wrapper.

use std::path::Path;

pub use agent_bridle::policy::{CapabilityClass, PolicyFile, PolicySet, Verdict};

use crate::agentic::DenialKind;

/// The four verdicts in load order — every store read walks exactly these.
const VERDICTS: [Verdict; 4] = [
    Verdict::Deny,
    Verdict::Passkey,
    Verdict::Ask,
    Verdict::Approve,
];

/// Map a newt permission axis onto the policy contract's capability class.
/// `None` for axes the durable store does not cover (a remote MCP tool leash is
/// name-based, not a fs/exec/net capability — it is never durably stored here).
pub fn class_for(kind: DenialKind) -> Option<CapabilityClass> {
    match kind {
        DenialKind::Exec => Some(CapabilityClass::Exec),
        DenialKind::FsRead | DenialKind::FsWrite => Some(CapabilityClass::Fs),
        DenialKind::Net => Some(CapabilityClass::Net),
        _ => None,
    }
}

/// Build a [`PolicySet`] from the four verdict files' contents (pure — the
/// unit-testable core). Each entry is `(verdict, Some(toml))` for a present
/// file or `(verdict, None)` for a missing one. A malformed file is SKIPPED
/// (its verdict stays empty) with the error pushed to `warnings` — a bad policy
/// file must never break startup, only lose that file's rules loudly.
pub fn build_store(files: &[(Verdict, Option<String>)]) -> (PolicySet, Vec<String>) {
    let mut set = PolicySet::default();
    let mut warnings = Vec::new();
    for (verdict, contents) in files {
        let Some(text) = contents else { continue };
        match PolicyFile::parse(text) {
            Ok(file) => {
                set.files.insert(*verdict, file);
            }
            Err(e) => warnings.push(format!("{}: {e}", verdict.filename())),
        }
    }
    (set, warnings)
}

/// Load the store from `<config dir>/ocap/*.toml` (the fs wrapper around
/// [`build_store`]). `config_path` is the config FILE path (e.g.
/// `~/.newt/config.toml`); the store lives beside it under `ocap/`. A missing
/// file = empty policy of that verdict.
pub fn load_store(config_path: &Path) -> (PolicySet, Vec<String>) {
    let dir = config_path.with_file_name("ocap");
    let files: Vec<(Verdict, Option<String>)> = VERDICTS
        .iter()
        .map(|&v| {
            let path = dir.join(v.filename());
            (v, std::fs::read_to_string(&path).ok())
        })
        .collect();
    build_store(&files)
}

/// The durable verdict for a permission request, if any (the one question the
/// gate asks the store). `None` ⇒ no durable policy ⇒ the gate prompts.
/// Matching is exact on the target string; a gate with richer matching
/// normalizes the target before calling.
pub fn evaluate_request(set: &PolicySet, kind: DenialKind, target: &str) -> Option<Verdict> {
    set.evaluate(class_for(kind)?, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(approve: &str, deny: &str) -> PolicySet {
        build_store(&[
            (Verdict::Approve, Some(approve.to_string())),
            (Verdict::Deny, Some(deny.to_string())),
        ])
        .0
    }

    #[test]
    fn class_mapping_covers_the_durable_axes() {
        assert_eq!(class_for(DenialKind::Exec), Some(CapabilityClass::Exec));
        assert_eq!(class_for(DenialKind::FsRead), Some(CapabilityClass::Fs));
        assert_eq!(class_for(DenialKind::FsWrite), Some(CapabilityClass::Fs));
        assert_eq!(class_for(DenialKind::Net), Some(CapabilityClass::Net));
    }

    #[test]
    fn evaluate_applies_the_contract_precedence() {
        // A durable approve pre-answers a prompt; a durable deny outranks it
        // (deny > … > approve — bridle's precedence law).
        let s = store(
            "[[exec]]\ntarget = \"cargo\"\n",
            "[[exec]]\ntarget = \"rm\"\n",
        );
        assert_eq!(
            evaluate_request(&s, DenialKind::Exec, "cargo"),
            Some(Verdict::Approve)
        );
        assert_eq!(
            evaluate_request(&s, DenialKind::Exec, "rm"),
            Some(Verdict::Deny)
        );
        // No durable policy → None → the gate prompts.
        assert_eq!(evaluate_request(&s, DenialKind::Exec, "python3"), None);
        // Class-scoped: an exec approve doesn't answer an fs question.
        assert_eq!(evaluate_request(&s, DenialKind::FsWrite, "cargo"), None);
    }

    #[test]
    fn fs_axis_matches_both_read_and_write_against_the_fs_class() {
        let s = build_store(&[(
            Verdict::Approve,
            Some("[[fs]]\npath = \"/ws\"\nwrite = true\n".to_string()),
        )])
        .0;
        assert_eq!(
            evaluate_request(&s, DenialKind::FsRead, "/ws"),
            Some(Verdict::Approve)
        );
        assert_eq!(
            evaluate_request(&s, DenialKind::FsWrite, "/ws"),
            Some(Verdict::Approve)
        );
    }

    #[test]
    fn a_malformed_file_is_skipped_loudly_not_fatal() {
        let (set, warnings) = build_store(&[
            (
                Verdict::Approve,
                Some("[[exec]]\ntarget = \"cargo\"\n".to_string()),
            ),
            (
                Verdict::Deny,
                Some("[[exec]]\ntarget=\"rm\"\nbad_key=1\n".to_string()),
            ),
        ]);
        // The good file loaded…
        assert_eq!(
            evaluate_request(&set, DenialKind::Exec, "cargo"),
            Some(Verdict::Approve)
        );
        // …the malformed one is absent (not fatal) and reported.
        assert!(
            warnings.iter().any(|w| w.contains("deny.toml")),
            "{warnings:?}"
        );
        assert_eq!(evaluate_request(&set, DenialKind::Exec, "rm"), None);
    }

    #[test]
    fn missing_files_yield_an_empty_store() {
        let (set, warnings) = build_store(&[(Verdict::Approve, None), (Verdict::Deny, None)]);
        assert!(warnings.is_empty());
        assert_eq!(evaluate_request(&set, DenialKind::Exec, "anything"), None);
    }
}
