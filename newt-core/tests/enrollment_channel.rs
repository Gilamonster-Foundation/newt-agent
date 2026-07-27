//! Enrollment staging → terminal promotion (#1369).
//!
//! The load-bearing property is a negative one: a web actor that fully controls
//! the staging call still cannot confer authority on itself. Most of this suite
//! is therefore about what does *not* happen — nothing durable is written until
//! a terminal confirmation, and a confirmation spends its candidate.

use agent_mesh_protocol::UserKey;
use newt_core::credential_registry::{append_credential, load_credentials, resolve_credential};
use newt_core::enrollment::{answer_enrollment_request_as, EnrollmentCandidate};
use newt_core::ConversationStore;

/// Five minutes in nanoseconds — the staging TTL, mirrored here because the
/// store's constant is crate-private.
const TTL_NANOS: i64 = 5 * 60 * 1_000_000_000;

struct Fixture {
    root: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    config_root: tempfile::TempDir,
    store: ConversationStore,
    conversation: String,
    key: UserKey,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let _workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), _workspace.path(), 100).unwrap();
    let conversation = store.create("enroll", None).unwrap();
    Fixture {
        root,
        _workspace,
        config_root,
        store,
        conversation,
        key: UserKey::generate(),
    }
}

impl Fixture {
    fn config_path(&self) -> std::path::PathBuf {
        self.config_root.path().join("config.toml")
    }

    fn stage(&self) -> String {
        let candidate = EnrollmentCandidate {
            credential_id_handle: "cred-abc".into(),
            cose_pubkey: "cG9zdC1rZXk=".into(),
            cose_alg: -7,
            mesh_agent_fingerprint: "agent-fp".into(),
            transcript_id: "tx-1".into(),
        };
        self.store
            .publish_enrollment_candidate(
                &self.conversation,
                &serde_json::to_string(&candidate).unwrap(),
            )
            .unwrap()
    }

    fn promote(&self, request_id: &str) -> anyhow::Result<Option<std::path::PathBuf>> {
        answer_enrollment_request_as(
            &self.store,
            &self.conversation,
            request_id,
            &self.config_path(),
            "operator",
            7,
            &self.key,
        )
    }

    /// Rows the registry retains after fail-closed verification.
    fn enrolled(&self) -> usize {
        load_credentials(&self.config_path(), Some(&self.key.public()))
            .0
            .len()
    }
}

#[test]
fn a_confirmed_candidate_becomes_a_verifiable_binding() {
    let f = fixture();
    let request_id = f.stage();

    let pending = f
        .store
        .pending_enrollment_candidate(&f.conversation)
        .unwrap()
        .expect("staged candidate is visible to the terminal");
    assert_eq!(pending.request_id, request_id);

    assert!(
        f.promote(&request_id).unwrap().is_some(),
        "terminal confirm promotes"
    );

    let (registry, warnings) = load_credentials(&f.config_path(), Some(&f.key.public()));
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let issuer = f.key.public().fingerprint().hex();
    assert!(
        resolve_credential(&registry, &issuer, "operator", "cred-abc").is_some(),
        "the promoted row must verify on read"
    );
}

/// The whole point of the split: staging alone writes nothing durable, so a web
/// actor cannot enroll itself no matter how many candidates it stages.
#[test]
fn staging_alone_confers_nothing() {
    let f = fixture();
    for _ in 0..3 {
        f.stage();
    }
    assert_eq!(f.enrolled(), 0, "no confirmation, no authority");
    assert!(
        !f.config_root.path().join("ocap/credentials.d").exists(),
        "staging must not even create the registry directory"
    );
}

/// A replayed confirmation must not enroll a second credential.
#[test]
fn confirmation_spends_its_candidate() {
    let f = fixture();
    let request_id = f.stage();

    assert!(f.promote(&request_id).unwrap().is_some());
    assert!(
        f.promote(&request_id).unwrap().is_none(),
        "a spent candidate must never promote again"
    );
    assert_eq!(f.enrolled(), 1);
    assert!(
        f.store
            .pending_enrollment_candidate(&f.conversation)
            .unwrap()
            .is_none(),
        "a taken candidate is no longer pending"
    );
}

#[test]
fn declined_and_expired_candidates_never_promote() {
    let f = fixture();
    let declined = f.stage();
    assert!(f
        .store
        .decline_enrollment_candidate(&f.conversation, &declined)
        .unwrap());
    assert!(
        !f.store
            .decline_enrollment_candidate(&f.conversation, &declined)
            .unwrap(),
        "declining twice is not a second retirement"
    );
    assert!(
        f.promote(&declined).unwrap().is_none(),
        "declined must not promote"
    );

    let mut f = fixture();
    f.store.set_claim_clock_for_test(|| 0);
    let stale = f.stage();
    assert!(f
        .store
        .pending_enrollment_candidate(&f.conversation)
        .unwrap()
        .is_some());
    f.store.set_claim_clock_for_test(|| TTL_NANOS + 1);
    assert!(
        f.store
            .pending_enrollment_candidate(&f.conversation)
            .unwrap()
            .is_none(),
        "an aged-out candidate is not renderable"
    );
    assert!(
        f.promote(&stale).unwrap().is_none(),
        "an aged-out candidate is gone"
    );
    assert_eq!(f.enrolled(), 0);
}

#[test]
fn an_unknown_request_id_promotes_nothing() {
    let f = fixture();
    f.stage();
    assert!(
        f.promote("not-a-real-request").unwrap().is_none(),
        "a guessed request id must not promote the staged candidate"
    );
    assert_eq!(f.enrolled(), 0);
}

/// The workspace fence every store table carries: another workspace can neither
/// see nor spend this workspace's candidate.
#[test]
fn a_foreign_workspace_can_neither_see_nor_take() {
    let f = fixture();
    let request_id = f.stage();

    let other_workspace = tempfile::tempdir().unwrap();
    let other = ConversationStore::new(f.root.path(), other_workspace.path(), 100).unwrap();
    assert!(other
        .pending_enrollment_candidate(&f.conversation)
        .unwrap()
        .is_none());
    assert!(other
        .take_enrollment_candidate(&f.conversation, &request_id)
        .unwrap()
        .is_none());
    // Still spendable by its rightful owner.
    assert!(f.promote(&request_id).unwrap().is_some());
}

/// `subject` names a file, so a traversing subject would let a promotion write
/// outside the registry directory.
#[test]
fn append_refuses_an_unsafe_subject() {
    let f = fixture();
    let candidate = EnrollmentCandidate {
        credential_id_handle: "cred-abc".into(),
        cose_pubkey: "a2V5".into(),
        cose_alg: -7,
        mesh_agent_fingerprint: "fp".into(),
        transcript_id: "tx".into(),
    };
    for subject in ["../escape", "a/b", "", ".hidden", "sub ject"] {
        assert!(
            append_credential(
                &f.config_path(),
                subject,
                candidate.clone().into_record(1),
                &f.key,
            )
            .is_err(),
            "subject {subject:?} must be refused"
        );
    }
}

#[test]
fn append_refuses_a_duplicate_handle_and_a_foreign_bundle() {
    let f = fixture();
    let request_id = f.stage();
    assert!(f.promote(&request_id).unwrap().is_some());

    // Same handle again — promotion refuses rather than shadowing the row that
    // is already enrolled.
    let again = f.stage();
    assert!(
        f.promote(&again).is_err(),
        "a duplicate handle must not append"
    );

    // A different operator's key must not write into this bundle.
    let stranger = UserKey::generate();
    let candidate = EnrollmentCandidate {
        credential_id_handle: "cred-other".into(),
        cose_pubkey: "a2V5".into(),
        cose_alg: -7,
        mesh_agent_fingerprint: "fp".into(),
        transcript_id: "tx".into(),
    };
    assert!(
        append_credential(
            &f.config_path(),
            "operator",
            candidate.into_record(1),
            &stranger,
        )
        .is_err(),
        "a foreign issuer must not append to an existing bundle"
    );
    assert_eq!(f.enrolled(), 1);
}
