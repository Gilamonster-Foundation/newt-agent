//! Host-side commit policy is independent of native Git argument parsing.
//! Transport and actual Git hook effects are grounded by the consumer's
//! subprocess tests; these cases cover the publication state they must obey.

use agent_toolchain::native_git::{CommitBroker, CommitPolicy};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Policy {
    signed: bool,
    fail_signing: bool,
    commits: AtomicUsize,
}

impl CommitPolicy for Policy {
    fn finalize_message(&self, message: &str) -> Result<String, String> {
        let message = message.trim_end().trim_end_matches("\n\nHarness: test");
        Ok(format!("{message}\n\nHarness: test\n"))
    }

    fn signing_required(&self) -> bool {
        self.signed
    }

    fn sign_commit(&self, _payload: &[u8]) -> Result<String, String> {
        if self.fail_signing {
            Err("signer unavailable".into())
        } else {
            Ok("-----BEGIN SSH SIGNATURE-----\nsignature\n-----END SSH SIGNATURE-----\n".into())
        }
    }

    fn committed(&self) {
        self.commits.fetch_add(1, Ordering::Relaxed);
    }
}

fn policy(signed: bool) -> Arc<Policy> {
    Arc::new(Policy {
        signed,
        fail_signing: false,
        commits: AtomicUsize::new(0),
    })
}

fn payload(message: &str) -> Vec<u8> {
    format!(
        "tree 1111111111111111111111111111111111111111\nauthor Original Author <author@example.invalid> 1 +0000\ncommitter Operator <operator@example.invalid> 2 +0000\n\n{message}"
    )
    .into_bytes()
}

#[test]
fn attribution_is_checked_again_after_git_and_repository_hooks() {
    let policy = policy(false);
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    assert!(broker
        .prepare_ref_update("refs/heads/task", &payload("hook removed attribution\n"))
        .is_err());
    broker
        .prepare_ref_update("refs/heads/task", &payload(&message))
        .unwrap();
    assert_eq!(policy.commits.load(Ordering::Relaxed), 0);
}

#[test]
fn required_signing_cannot_be_skipped_or_changed_before_publication() {
    let policy = policy(true);
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    let unsigned = payload(&message);
    assert!(broker
        .prepare_ref_update("refs/heads/task", &unsigned)
        .is_err());
    let signature = broker.sign(&unsigned).unwrap();
    let signed = agent_toolchain::native_git::with_signature(&unsigned, &signature);
    broker
        .prepare_ref_update("refs/heads/task", &signed)
        .unwrap();
    let mut changed = signed.clone();
    changed.extend_from_slice(b"changed\n");
    assert!(broker
        .prepare_ref_update("refs/heads/task", &changed)
        .is_err());
    assert!(broker
        .prepare_ref_update("refs/heads/other", &signed)
        .is_err());
}

#[test]
fn author_and_committer_bytes_are_not_rewritten_by_attribution_or_signing() {
    let policy = policy(true);
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    let unsigned = payload(&message);
    let signature = broker.sign(&unsigned).unwrap();
    let signed = agent_toolchain::native_git::with_signature(&unsigned, &signature);
    assert!(signed.starts_with(unsigned.split(|byte| *byte == b'\n').next().unwrap()));
    assert!(String::from_utf8(signed)
        .unwrap()
        .contains("author Original Author <author@example.invalid> 1 +0000\ncommitter Operator <operator@example.invalid> 2 +0000\n"));
}

#[test]
fn failed_signing_or_aborted_transaction_does_not_consume_contributors() {
    let policy = Arc::new(Policy {
        signed: true,
        fail_signing: true,
        commits: AtomicUsize::new(0),
    });
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    let unsigned = payload(&message);
    assert!(broker.sign(&unsigned).is_err());
    assert!(broker
        .prepare_ref_update("refs/heads/task", &unsigned)
        .is_err());
    assert!(broker.committed("refs/heads/task", &unsigned).is_err());
    assert_eq!(policy.commits.load(Ordering::Relaxed), 0);
}

#[test]
fn only_confirmed_matching_ref_update_consumes_contributors_once() {
    let policy = policy(false);
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    let commit = payload(&message);
    assert!(broker.committed("refs/heads/task", &commit).is_err());
    broker
        .prepare_ref_update("refs/heads/task", &commit)
        .unwrap();
    assert!(broker.committed("refs/heads/other", &commit).is_err());
    broker.committed("refs/heads/task", &commit).unwrap();
    assert!(broker.committed("refs/heads/task", &commit).is_err());
    assert_eq!(policy.commits.load(Ordering::Relaxed), 1);
}

#[test]
fn aborted_preparation_cannot_be_reported_as_a_committed_update() {
    let policy = policy(false);
    let mut broker = CommitBroker::new(policy.clone(), "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    let commit = payload(&message);
    broker
        .prepare_ref_update("refs/heads/task", &commit)
        .unwrap();
    broker.aborted();
    assert!(broker.committed("refs/heads/task", &commit).is_err());
    assert_eq!(policy.commits.load(Ordering::Relaxed), 0);
}

#[test]
fn signer_rejects_noncommit_and_already_signed_payloads() {
    let policy = policy(true);
    let mut broker = CommitBroker::new(policy, "refs/heads/task");
    let message = broker.prepare_message("subject\n").unwrap();
    assert!(broker
        .sign(format!("arbitrary payload\n\n{message}").as_bytes())
        .is_err());
    let unsigned = payload(&message);
    let signed = agent_toolchain::native_git::with_signature(&unsigned, "existing signature");
    assert!(broker.sign(&signed).is_err());
}
