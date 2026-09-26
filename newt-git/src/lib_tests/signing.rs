//! Harness commit signing, grounded against real `git verify-commit`.
//!
//! `commit_signing`'s unit tests check the SSHSIG bytes it builds; only git
//! itself can say that the header placement and armor are what it verifies.

use super::*;
use newt_core::commit_signing::{generate_harness_key, session_signer, HarnessSshSigner};

fn commit_one(t: &LocalGitTool, dir: &Path) -> Result<String, String> {
    t.dispatch(
        "init",
        &serde_json::json!({}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )?;
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["a.txt"]}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )?;
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "C1"}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )
}

#[test]
fn a_harness_signed_commit_verifies_with_git() {
    let dir = tempfile::tempdir().unwrap();
    let keys = tempfile::tempdir().unwrap();
    let key = keys.path().join("harness-signing.pem");
    let public = generate_harness_key(&key).unwrap();
    let mut t = tool(dir.path());
    t.signer = Some(std::sync::Arc::new(HarnessSshSigner::load(&key).unwrap()));
    commit_one(&t, dir.path()).unwrap();

    let allowed = keys.path().join("allowed_signers");
    std::fs::write(&allowed, format!("bot@example.com {public}\n")).unwrap();
    let out = git_cmd(dir.path())
        .arg("-c")
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .args(["verify-commit", "HEAD"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("Good \"git\" signature"), "{stderr}");
}

#[test]
fn a_signer_that_cannot_sign_leaves_no_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut identity = newt_core::AgentIdentity::default();
    identity.git.signing = newt_core::agent_identity::SigningMode::Harness; // no key
    let mut t = tool(dir.path());
    t.signer = session_signer(&identity);
    let error = commit_one(&t, dir.path()).unwrap_err();
    assert!(error.contains("commit signing failed"), "{error}");
    let head = git_cmd(dir.path())
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(!head.status.success(), "an unsigned commit was written");
}
