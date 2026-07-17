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
//! Three rules from the contract (bridle `policy`), enforced by construction:
//! - **Precedence:** deny > passkey > ask > approve — the most restrictive
//!   durable verdict wins. [`PolicySet::evaluate`] applies it.
//! - **No match ⇒ fall through:** [`evaluate_request`] returns `None`, and the
//!   gate uses its interactive prompt / default-deny floor. Durable policy only
//!   ever *narrows or pre-answers*; it never widens the floor.
//! - **The loosening verdict is signed** (#1207 / bridle #226): every
//!   `approve.toml` entry must carry a valid Ed25519 `sig` by the operator's
//!   root key. [`verify_approves`] drops unsigned/tampered approves LOUDLY at
//!   load — fail-closed to "no durable grant" (the gate prompts). With no root
//!   key available, ALL approves drop. The narrowing verdicts (deny/ask/
//!   passkey) load unsigned: forging them can only narrow, and dropping a deny
//!   over a bad signature would widen — exactly backwards.
//!
//! Split for the mocked-unit tier (no real fs in unit tests): [`build_store`] +
//! [`verify_approves`] are pure (contents + injected verifier); [`load_store`]
//! is the thin fs wrapper.

use std::path::Path;

pub use agent_bridle::policy::{
    ApproveVerifier, CapabilityClass, Ed25519ApproveVerifier, ExecEntry, FsEntry, NetEntry,
    PolicyFile, PolicySet, Verdict,
};

use crate::agentic::DenialKind;

/// The four verdicts in load order — every store read walks exactly these.
pub const VERDICTS: [Verdict; 4] = [
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

/// Sanitize the LOOSENING verdict (pure; #1207): run `approve.toml` through
/// bridle's fail-closed [`PolicyFile::verified_approves`] with the trusted
/// verifier. `None` (no root key this session) drops ALL approve entries with
/// one loud warning — an unverifiable grant is no grant. The narrowing
/// verdicts pass through untouched (see the module docs for why).
pub fn verify_approves(
    mut set: PolicySet,
    verifier: Option<&dyn ApproveVerifier>,
) -> (PolicySet, Vec<String>) {
    let Some(approves) = set.files.remove(&Verdict::Approve) else {
        return (set, Vec::new());
    };
    match verifier {
        Some(v) => {
            let (kept, warnings) = approves.verified_approves(v);
            set.files.insert(Verdict::Approve, kept);
            (set, warnings)
        }
        None => {
            let dropped = approves.exec.len() + approves.fs.len() + approves.net.len();
            let warnings = if dropped == 0 {
                Vec::new()
            } else {
                vec![format!(
                    "approve.toml: no root signing key available — {dropped} \
                     durable grant(s) dropped (fail-closed; the gate will prompt)"
                )]
            };
            (set, warnings)
        }
    }
}

/// Load the store from `<config dir>/ocap/*.toml` (the fs wrapper around
/// [`build_store`] + [`verify_approves`]). `config_path` is the config FILE
/// path (e.g. `~/.newt/config.toml`); the store lives beside it under `ocap/`.
/// A missing file = empty policy of that verdict. `root_verifying_key` is the
/// operator's 32-byte Ed25519 root public key (newt-identity) that approve
/// signatures must verify under; `None` fail-closes every durable grant.
pub fn load_store(
    config_path: &Path,
    root_verifying_key: Option<[u8; 32]>,
) -> (PolicySet, Vec<String>) {
    let dir = config_path.with_file_name("ocap");
    let files: Vec<(Verdict, Option<String>)> = VERDICTS
        .iter()
        .map(|&v| {
            let path = dir.join(v.filename());
            (v, std::fs::read_to_string(&path).ok())
        })
        .collect();
    let (set, mut warnings) = build_store(&files);
    let verifier = root_verifying_key.map(|verifying_key| Ed25519ApproveVerifier { verifying_key });
    let (set, mut sig_warnings) =
        verify_approves(set, verifier.as_ref().map(|v| v as &dyn ApproveVerifier));
    warnings.append(&mut sig_warnings);
    (set, warnings)
}

/// The durable verdict for a permission request, if any (the one question the
/// gate asks the store). `None` ⇒ no durable policy ⇒ the gate prompts.
/// Matching is exact on the target string; a gate with richer matching
/// normalizes the target before calling.
pub fn evaluate_request(set: &PolicySet, kind: DenialKind, target: &str) -> Option<Verdict> {
    set.evaluate(class_for(kind)?, target)
}

/// Bless an `approve.toml` (pure; the write half of #1207): re-sign EVERY entry
/// with the operator's root key — the ceremony behind `newt doctor --sign-ocap`,
/// where a present human explicitly vouches for the file as it stands (that
/// presence + explicit command IS the authorization; hand-edited entries become
/// valid grants here and nowhere else).
///
/// The danger invariant binds even here: a high-danger target (per the caller's
/// danger table, passed as a predicate per the bridle contract) is REFUSED a
/// signature and reported — `validate_approve` is the contract's mandatory
/// pre-persist check, and blessing must not launder an interpreter grant into
/// the store. Refused entries keep whatever `sig` they had (an invalid one
/// still drops at load, fail-closed).
///
/// Returns `(signed_count, refused)` — refusal strings are operator-facing.
pub fn sign_approves(
    file: &mut PolicyFile,
    is_high_danger: impl Fn(CapabilityClass, &str) -> bool,
    sign: impl Fn(&[u8]) -> [u8; 64],
) -> (usize, Vec<String>) {
    let mut signed = 0;
    let mut refused = Vec::new();
    let mut bless =
        |class: CapabilityClass, what: &str, payload: Vec<u8>, sig: &mut Option<String>| {
            match PolicySet::validate_approve(class, what, &is_high_danger) {
                Ok(()) => {
                    *sig = Some(agent_bridle::policy::hex_encode(&sign(&payload)));
                    signed += 1;
                }
                Err(reason) => refused.push(reason),
            }
        };
    for e in &mut file.exec {
        let payload = e.signing_payload();
        let target = e.target.clone();
        bless(CapabilityClass::Exec, &target, payload, &mut e.sig);
    }
    for e in &mut file.fs {
        let payload = e.signing_payload();
        let path = e.path.clone();
        bless(CapabilityClass::Fs, &path, payload, &mut e.sig);
    }
    for e in &mut file.net {
        let payload = e.signing_payload();
        let host = e.host.clone();
        bless(CapabilityClass::Net, &host, payload, &mut e.sig);
    }
    (signed, refused)
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

    /// The pure verifier double: accepts exactly one signature value.
    struct SigEquals(&'static [u8]);
    impl ApproveVerifier for SigEquals {
        fn verify(&self, _payload: &[u8], sig: &[u8]) -> Result<(), String> {
            (sig == self.0).then_some(()).ok_or("bad signature".into())
        }
    }

    /// #1207 asymmetry law: an unsigned approve is DROPPED (fail-closed, loud);
    /// an unsigned deny SURVIVES (narrowing verdicts load unsigned — dropping a
    /// deny over a missing sig would widen).
    #[test]
    fn unsigned_approve_drops_but_unsigned_deny_survives() {
        let (set, _) = build_store(&[
            (
                Verdict::Approve,
                Some("[[exec]]\ntarget = \"git\"\n".to_string()),
            ),
            (
                Verdict::Deny,
                Some("[[exec]]\ntarget = \"rm\"\n".to_string()),
            ),
        ]);
        let (set, warnings) = verify_approves(set, Some(&SigEquals(&[1])));
        // The unsigned approve is gone: no durable grant, the gate prompts.
        assert_eq!(evaluate_request(&set, DenialKind::Exec, "git"), None);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("git") && w.contains("unsigned")),
            "{warnings:?}"
        );
        // The unsigned deny still binds.
        assert_eq!(
            evaluate_request(&set, DenialKind::Exec, "rm"),
            Some(Verdict::Deny)
        );
    }

    /// #1207: a validly-signed approve survives verification and answers the
    /// gate; a bad signature drops.
    #[test]
    fn signed_approve_survives_and_bad_sig_drops() {
        let (set, _) = build_store(&[(
            Verdict::Approve,
            Some(
                "[[exec]]\ntarget = \"cargo\"\nsig = \"0a\"\n\
                 [[net]]\nhost = \"crates.io\"\nsig = \"ff\"\n"
                    .to_string(),
            ),
        )]);
        let (set, warnings) = verify_approves(set, Some(&SigEquals(&[0x0a])));
        assert_eq!(
            evaluate_request(&set, DenialKind::Exec, "cargo"),
            Some(Verdict::Approve),
            "the validly-signed grant answers the gate"
        );
        assert_eq!(evaluate_request(&set, DenialKind::Net, "crates.io"), None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// #1207 fail-closed floor: with NO root key, every durable grant drops
    /// (one loud warning) — an unverifiable grant is no grant. An empty
    /// approve file warns nothing.
    #[test]
    fn no_root_key_drops_all_approves_fail_closed() {
        let (set, _) = build_store(&[(
            Verdict::Approve,
            Some("[[exec]]\ntarget = \"cargo\"\nsig = \"0a\"\n".to_string()),
        )]);
        let (set, warnings) = verify_approves(set, None);
        assert_eq!(evaluate_request(&set, DenialKind::Exec, "cargo"), None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no root signing key"), "{warnings:?}");

        let (empty, warnings) = verify_approves(PolicySet::default(), None);
        assert!(warnings.is_empty(), "nothing to drop, nothing to warn");
        assert_eq!(evaluate_request(&empty, DenialKind::Exec, "cargo"), None);
    }

    /// #1207 write half: blessing signs every non-dangerous entry with the
    /// canonical payload (round-trips through the verify path), REFUSES
    /// high-danger targets (the contract's validate_approve check), and is
    /// idempotent (re-blessing re-signs).
    #[test]
    fn sign_approves_blesses_safe_entries_and_refuses_high_danger() {
        let mut file = PolicyFile::parse(
            "[[exec]]\ntarget = \"cargo\"\n\
             [[exec]]\ntarget = \"bash\"\n\
             [[fs]]\npath = \"/ws\"\nwrite = true\n",
        )
        .unwrap();
        let is_high = |class: CapabilityClass, target: &str| {
            class == CapabilityClass::Exec && target == "bash"
        };
        // A "signature" the double can check: payload length as the first byte.
        let sign = |payload: &[u8]| {
            let mut sig = [0u8; 64];
            sig[0] = payload.len() as u8;
            sig
        };
        let (signed, refused) = sign_approves(&mut file, is_high, sign);
        assert_eq!(signed, 2, "cargo + the fs path");
        assert_eq!(refused.len(), 1);
        assert!(refused[0].contains("bash"), "{refused:?}");
        assert!(file.exec[0].sig.is_some());
        assert!(file.exec[1].sig.is_none(), "the interpreter stays unsigned");
        assert!(file.fs[0].sig.is_some());

        // The blessed file survives the verify path with the matching checker;
        // the refused (unsigned) interpreter entry drops fail-closed.
        struct LenChecker;
        impl ApproveVerifier for LenChecker {
            fn verify(&self, payload: &[u8], sig: &[u8]) -> Result<(), String> {
                (sig.first() == Some(&(payload.len() as u8)))
                    .then_some(())
                    .ok_or("mismatch".into())
            }
        }
        let mut set = PolicySet::default();
        set.files.insert(Verdict::Approve, file);
        let (set, warnings) = verify_approves(set, Some(&LenChecker));
        assert_eq!(
            evaluate_request(&set, DenialKind::Exec, "cargo"),
            Some(Verdict::Approve)
        );
        assert_eq!(
            evaluate_request(&set, DenialKind::FsWrite, "/ws"),
            Some(Verdict::Approve)
        );
        assert_eq!(evaluate_request(&set, DenialKind::Exec, "bash"), None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
