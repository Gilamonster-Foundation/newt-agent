//! Policy at native Git's commit boundaries, independent of its command line.
//!
//! Git owns argument parsing, the index, author selection, and repository hooks.
//! The embedding harness owns attribution and signing keys. A private transport
//! must bind this state to one authorized Git invocation and verify ref events;
//! these methods do not authenticate a caller or grant filesystem authority.

pub mod helper;

/// Bounds both a message and a complete candidate retained by the broker.
pub const MAX_COMMIT_BYTES: usize = 256 * 1024;

/// Harness-owned policy. Implementations keep signing keys out of the child.
pub trait CommitPolicy: Send + Sync {
    /// Capture mutable session bookkeeping at the command boundary. Policies
    /// that are already immutable may keep the default shared instance.
    fn snapshot_for_commit(&self) -> Option<std::sync::Arc<dyn CommitPolicy>> {
        None
    }

    /// Canonical, idempotent attribution applied to Git's message.
    fn finalize_message(&self, message: &str) -> Result<String, String>;

    /// Whether the operator requires a signature for this invocation.
    fn signing_required(&self) -> bool;

    /// Sign the exact unsigned commit bytes, without changing their headers.
    fn sign_commit(&self, payload: &[u8]) -> Result<String, String>;

    /// A matching, prepared ref transaction actually committed.
    fn committed(&self);
}

/// A single native commit's message/signature/publication state.
///
/// The caller supplies the already-authorized destination ref. It must observe
/// Git's real prepared/committed transaction boundaries over a private channel;
/// model-provided output, a changed HEAD, and a successful signing request are
/// not commit-success evidence. No contributor is consumed before confirmation.
pub struct CommitBroker {
    policy: std::sync::Arc<dyn CommitPolicy>,
    reference: String,
    signed_commit: Option<Vec<u8>>,
    prepared_commit: Option<Vec<u8>>,
    completed: bool,
}

impl CommitBroker {
    /// Bind a broker to the policy and destination authorized by its caller.
    pub fn new(policy: std::sync::Arc<dyn CommitPolicy>, reference: impl Into<String>) -> Self {
        Self {
            policy,
            reference: reference.into(),
            signed_commit: None,
            prepared_commit: None,
            completed: false,
        }
    }

    /// Finalize the message Git assembled, before its editor/message hooks run.
    /// The final commit is checked again; this early hook is not the boundary.
    pub fn prepare_message(&self, message: &str) -> Result<String, String> {
        self.ensure_active()?;
        check_size(message.as_bytes())?;
        self.policy.finalize_message(message)
    }

    /// Sign only a commit whose final message satisfies the harness policy.
    /// Record the exact expected signed object for the publication check.
    pub fn sign(&mut self, payload: &[u8]) -> Result<String, String> {
        self.ensure_active()?;
        self.check_message(payload)?;
        let headers = headers(payload)?;
        if !headers.starts_with(b"tree ")
            || headers
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"author "))
                .count()
                != 1
            || headers
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"committer "))
                .count()
                != 1
            || headers
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 "))
        {
            return Err("signing request is not an unsigned Git commit".into());
        }
        if !self.policy.signing_required() {
            return Err("commit signing is not configured for this invocation".into());
        }
        let signature = self.policy.sign_commit(payload)?;
        self.signed_commit = Some(with_signature(payload, &signature));
        Ok(signature)
    }

    /// Validate the exact candidate before Git publishes it to the approved ref.
    /// A skipped signer, changed message, changed signature, or other ref fails.
    pub fn prepare_ref_update(&mut self, reference: &str, commit: &[u8]) -> Result<(), String> {
        self.ensure_active()?;
        if reference != self.reference {
            return Err("commit destination differs from the authorized reference".into());
        }
        self.check_message(commit)?;
        if self.policy.signing_required() && self.signed_commit.as_deref() != Some(commit) {
            return Err("commit does not carry the exact harness-approved signature".into());
        }
        self.prepared_commit = Some(commit.to_vec());
        Ok(())
    }

    /// Discard a prepared transaction that Git aborted. Signing alone never
    /// changes the harness's contributor-consumption state.
    pub fn aborted(&mut self) {
        self.prepared_commit = None;
    }

    /// Record a verified committed event once, for the exact prepared candidate.
    pub fn committed(&mut self, reference: &str, commit: &[u8]) -> Result<(), String> {
        self.ensure_active()?;
        if reference != self.reference || self.prepared_commit.as_deref() != Some(commit) {
            return Err("commit success has no matching prepared reference transaction".into());
        }
        self.completed = true;
        self.prepared_commit = None;
        self.policy.committed();
        Ok(())
    }

    fn ensure_active(&self) -> Result<(), String> {
        if self.completed {
            Err("native commit invocation is already complete".into())
        } else {
            Ok(())
        }
    }

    fn check_message(&self, commit: &[u8]) -> Result<(), String> {
        let offset = headers(commit)?.len();
        let message = std::str::from_utf8(&commit[offset + 2..])
            .map_err(|_| "native commit message is not UTF-8")?;
        if self.policy.finalize_message(message)? != message {
            return Err("native commit message changed after harness attribution".into());
        }
        Ok(())
    }
}

fn check_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_COMMIT_BYTES {
        Err("native commit exceeds the broker's bounded message size".into())
    } else {
        Ok(())
    }
}

fn headers(commit: &[u8]) -> Result<&[u8], String> {
    check_size(commit)?;
    commit
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|offset| &commit[..offset])
        .ok_or_else(|| "native commit has no message boundary".into())
}

/// Insert an armored signature as Git's final header, retaining every unsigned
/// byte, including author/committer identity and the original message.
#[must_use]
pub fn with_signature(commit: &[u8], armored: &str) -> Vec<u8> {
    let split = commit
        .windows(2)
        .position(|window| window == b"\n\n")
        .map_or(commit.len(), |at| at + 1);
    let header = format!("gpgsig {}\n", armored.trim_end().replace('\n', "\n "));
    let mut out = Vec::with_capacity(commit.len() + header.len());
    out.extend_from_slice(&commit[..split]);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&commit[split..]);
    out
}
