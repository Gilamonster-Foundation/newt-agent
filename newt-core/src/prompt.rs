//! Durable, addressable operator-prompt provenance.
//!
//! A chat transcript is a presentation that compaction may transform. These
//! types describe the immutable prompt receipt captured before model or tool
//! work begins. Exact bytes live in [`PromptReceipt`]; [`ActivePrompt`] and
//! [`TurnPromptContext`] make the distinction between a submitted harness
//! retry and the operator prompt it is retrying explicit in the type surface.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

const RAW_DIGEST_V1_PREFIX: &[u8] = b"newt-prompt-raw:v1";
const MODEL_DIGEST_V1_PREFIX: &[u8] = b"newt-prompt-model:v1";
const RECEIPT_HASH_V1_PREFIX: &[u8] = b"newt-prompt-receipt:v1";
const RECEIPT_HASH_V2_PREFIX: &[u8] = b"newt-prompt-receipt:v2";

/// Stable address of one immutable prompt receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PromptId(Uuid);

impl PromptId {
    /// Mint a new random prompt address.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from an already-minted UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The UUID portion of this prompt address.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for PromptId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PromptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "prompt:{}", self.0)
    }
}

impl FromStr for PromptId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Accept the UUID alone for database/tooling ergonomics, but always
        // render the capability-style `prompt:` address.
        let value = value.strip_prefix("prompt:").unwrap_or(value);
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for PromptId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PromptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Who submitted a receipt to the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOrigin {
    /// A prompt accepted directly from the operator.
    Operator,
    /// Harness-generated retry text explicitly descended from an operator
    /// prompt. It is recorded, but never becomes the active operator prompt.
    HarnessRetry,
}

impl PromptOrigin {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::HarnessRetry => "harness_retry",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "operator" => Ok(Self::Operator),
            "harness_retry" => Ok(Self::HarnessRetry),
            other => anyhow::bail!("unknown prompt origin `{other}`"),
        }
    }
}

/// Exact prompt material awaiting durable receipt creation.
///
/// `raw_text` is what ingress accepted; `model_text` is the byte-exact payload
/// the harness will send to inference. Chronological predecessor selection is
/// deliberately absent: the store assigns it automatically. Semantic ancestry
/// exists only when the caller chooses a continuation/retry constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPrompt {
    pub(crate) origin: PromptOrigin,
    pub(crate) raw_text: Vec<u8>,
    pub(crate) model_text: Vec<u8>,
    pub(crate) parent_prompt_id: Option<PromptId>,
}

impl NewPrompt {
    /// A new operator objective. Its root is the receipt itself and it has no
    /// semantic parent, even when another prompt precedes it chronologically.
    pub fn operator(raw_text: impl Into<Vec<u8>>, model_text: impl Into<Vec<u8>>) -> Self {
        Self {
            origin: PromptOrigin::Operator,
            raw_text: raw_text.into(),
            model_text: model_text.into(),
            parent_prompt_id: None,
        }
    }

    /// A new operator submission explicitly continuing an existing objective.
    /// The store validates the parent is in the same conversation/workspace and
    /// inherits that parent's objective root.
    pub fn operator_continuation(
        raw_text: impl Into<Vec<u8>>,
        model_text: impl Into<Vec<u8>>,
        parent_prompt_id: PromptId,
    ) -> Self {
        Self {
            origin: PromptOrigin::Operator,
            raw_text: raw_text.into(),
            model_text: model_text.into(),
            parent_prompt_id: Some(parent_prompt_id),
        }
    }

    /// Harness retry text explicitly attached to an existing operator
    /// objective. The parent/root relation is validated by the store.
    pub fn harness_retry(
        raw_text: impl Into<Vec<u8>>,
        model_text: impl Into<Vec<u8>>,
        parent_prompt_id: PromptId,
    ) -> Self {
        Self {
            origin: PromptOrigin::HarnessRetry,
            raw_text: raw_text.into(),
            model_text: model_text.into(),
            parent_prompt_id: Some(parent_prompt_id),
        }
    }

    pub fn origin(&self) -> PromptOrigin {
        self.origin
    }

    pub fn raw_text(&self) -> &[u8] {
        &self.raw_text
    }

    pub fn model_text(&self) -> &[u8] {
        &self.model_text
    }

    pub fn raw_text_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.raw_text)
    }

    pub fn model_text_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.model_text)
    }

    pub fn parent_prompt_id(&self) -> Option<PromptId> {
        self.parent_prompt_id
    }
}

/// Immutable durable receipt for one submitted prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReceipt {
    id: PromptId,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    previous_prompt_id: Option<PromptId>,
    parent_prompt_id: Option<PromptId>,
    root_prompt_id: PromptId,
    /// The nearest operator submission whose authority this receipt carries.
    /// Distinct from `root_prompt_id`: an operator clarification may remain in
    /// the original objective while becoming the authority for later retries.
    active_operator_id: Option<PromptId>,
    origin: PromptOrigin,
    raw_text: Vec<u8>,
    model_text: Vec<u8>,
    raw_digest: String,
    model_digest: String,
    receipt_hash: String,
    ts_claim: i64,
    encoding_version: i64,
}

impl PromptReceipt {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: PromptId,
        conversation_id: String,
        writer_fingerprint: String,
        seq: i64,
        previous_prompt_id: Option<PromptId>,
        parent_prompt_id: Option<PromptId>,
        root_prompt_id: PromptId,
        active_operator_id: PromptId,
        origin: PromptOrigin,
        raw_text: Vec<u8>,
        model_text: Vec<u8>,
        ts_claim: i64,
    ) -> Self {
        let raw_digest = digest_bytes(RAW_DIGEST_V1_PREFIX, &raw_text);
        let model_digest = digest_bytes(MODEL_DIGEST_V1_PREFIX, &model_text);
        let mut receipt = Self {
            id,
            conversation_id,
            writer_fingerprint,
            seq,
            previous_prompt_id,
            parent_prompt_id,
            root_prompt_id,
            active_operator_id: Some(active_operator_id),
            origin,
            raw_text,
            model_text,
            raw_digest,
            model_digest,
            receipt_hash: String::new(),
            ts_claim,
            encoding_version: 2,
        };
        receipt.receipt_hash = receipt.compute_receipt_hash_v2();
        receipt
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored_parts(
        id: PromptId,
        conversation_id: String,
        writer_fingerprint: String,
        seq: i64,
        previous_prompt_id: Option<PromptId>,
        parent_prompt_id: Option<PromptId>,
        root_prompt_id: PromptId,
        active_operator_id: Option<PromptId>,
        origin: PromptOrigin,
        raw_text: Vec<u8>,
        model_text: Vec<u8>,
        raw_digest: String,
        model_digest: String,
        receipt_hash: String,
        ts_claim: i64,
        encoding_version: i64,
    ) -> Self {
        Self {
            id,
            conversation_id,
            writer_fingerprint,
            seq,
            previous_prompt_id,
            parent_prompt_id,
            root_prompt_id,
            active_operator_id,
            origin,
            raw_text,
            model_text,
            raw_digest,
            model_digest,
            receipt_hash,
            ts_claim,
            encoding_version,
        }
    }

    pub fn id(&self) -> PromptId {
        self.id
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn writer_fingerprint(&self) -> &str {
        &self.writer_fingerprint
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    pub fn previous_prompt_id(&self) -> Option<PromptId> {
        self.previous_prompt_id
    }

    pub fn parent_prompt_id(&self) -> Option<PromptId> {
        self.parent_prompt_id
    }

    pub fn root_prompt_id(&self) -> PromptId {
        self.root_prompt_id
    }

    /// Persisted nearest operator authority. Version-1 receipts return `None`;
    /// the store reconstructs those through their validated parent chain.
    pub fn active_operator_id(&self) -> Option<PromptId> {
        self.active_operator_id
    }

    pub fn origin(&self) -> PromptOrigin {
        self.origin
    }

    pub fn raw_text(&self) -> &[u8] {
        &self.raw_text
    }

    pub fn model_text(&self) -> &[u8] {
        &self.model_text
    }

    pub fn raw_text_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.raw_text)
    }

    pub fn model_text_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.model_text)
    }

    pub fn raw_digest(&self) -> &str {
        &self.raw_digest
    }

    pub fn model_digest(&self) -> &str {
        &self.model_digest
    }

    pub fn receipt_hash(&self) -> &str {
        &self.receipt_hash
    }

    pub fn ts_claim(&self) -> i64 {
        self.ts_claim
    }

    /// Recompute both representation digests and the receipt hash. Store reads
    /// call this before returning data so corrupt/tampered bytes never become an
    /// authoritative active prompt.
    pub fn verify_integrity(&self) -> anyhow::Result<()> {
        let raw_digest = digest_bytes(RAW_DIGEST_V1_PREFIX, &self.raw_text);
        if raw_digest != self.raw_digest {
            anyhow::bail!("prompt {} raw-text digest mismatch", self.id);
        }
        let model_digest = digest_bytes(MODEL_DIGEST_V1_PREFIX, &self.model_text);
        if model_digest != self.model_digest {
            anyhow::bail!("prompt {} model-text digest mismatch", self.id);
        }
        let receipt_hash = match self.encoding_version {
            1 => {
                if self.active_operator_id.is_some() {
                    anyhow::bail!(
                        "prompt {} version-1 receipt carries unhashed active-operator metadata",
                        self.id
                    );
                }
                self.compute_receipt_hash_v1()
            }
            2 => {
                if self.active_operator_id.is_none() {
                    anyhow::bail!(
                        "prompt {} version-2 receipt is missing active operator",
                        self.id
                    );
                }
                self.compute_receipt_hash_v2()
            }
            other => anyhow::bail!(
                "prompt {} carries encoding_version {other}, which this newt does not understand",
                self.id
            ),
        };
        if receipt_hash != self.receipt_hash {
            anyhow::bail!("prompt {} receipt hash mismatch", self.id);
        }
        Ok(())
    }

    fn compute_receipt_hash_v1(&self) -> String {
        self.compute_receipt_hash(RECEIPT_HASH_V1_PREFIX, false)
    }

    fn compute_receipt_hash_v2(&self) -> String {
        self.compute_receipt_hash(RECEIPT_HASH_V2_PREFIX, true)
    }

    fn compute_receipt_hash(&self, prefix: &[u8], include_active_operator: bool) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(prefix);
        hash_field(&mut hasher, self.id.to_string().as_bytes());
        hash_field(&mut hasher, self.conversation_id.as_bytes());
        hash_field(&mut hasher, self.writer_fingerprint.as_bytes());
        hasher.update(&self.seq.to_le_bytes());
        hash_optional_id(&mut hasher, self.previous_prompt_id);
        hash_optional_id(&mut hasher, self.parent_prompt_id);
        hash_field(&mut hasher, self.root_prompt_id.to_string().as_bytes());
        if include_active_operator {
            hash_optional_id(&mut hasher, self.active_operator_id);
        }
        hash_field(&mut hasher, self.origin.as_db_str().as_bytes());
        hash_field(&mut hasher, self.raw_digest.as_bytes());
        hash_field(&mut hasher, self.model_digest.as_bytes());
        hasher.update(&self.ts_claim.to_le_bytes());
        hasher.update(&self.encoding_version.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }

    pub(crate) fn encoding_version(&self) -> i64 {
        self.encoding_version
    }

    #[cfg(test)]
    pub(crate) fn into_legacy_v1_for_test(mut self) -> Self {
        self.active_operator_id = None;
        self.encoding_version = 1;
        self.receipt_hash = self.compute_receipt_hash_v1();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_active_operator_for_test(mut self, active: PromptId) -> Self {
        self.active_operator_id = Some(active);
        self.encoding_version = 2;
        self.receipt_hash = self.compute_receipt_hash_v2();
        self
    }
}

/// A prompt snapshot safe to carry through compaction/retry paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePrompt(PromptReceipt);

impl ActivePrompt {
    pub(crate) fn new(receipt: PromptReceipt) -> Self {
        Self(receipt)
    }

    pub fn receipt(&self) -> &PromptReceipt {
        &self.0
    }

    pub fn id(&self) -> PromptId {
        self.0.id()
    }

    pub fn root_prompt_id(&self) -> PromptId {
        self.0.root_prompt_id()
    }

    pub fn model_text(&self) -> &[u8] {
        self.0.model_text()
    }

    pub fn model_text_utf8(&self) -> Result<&str, std::str::Utf8Error> {
        self.0.model_text_utf8()
    }

    pub fn model_digest(&self) -> &str {
        self.0.model_digest()
    }
}

/// Provenance for one harness turn.
///
/// For operator input, `submitted` and `active` are the same receipt. For a
/// harness retry, `submitted` is the retry receipt while `active` is the
/// nearest validated operator submission inherited from its parent. Objective
/// root lineage stays separate. This prevents retry prose from becoming
/// authority without discarding a later locked operator clarification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPromptContext {
    submitted: ActivePrompt,
    active: ActivePrompt,
}

impl TurnPromptContext {
    pub(crate) fn new(submitted: PromptReceipt, active: PromptReceipt) -> Self {
        Self {
            submitted: ActivePrompt::new(submitted),
            active: ActivePrompt::new(active),
        }
    }

    pub fn submitted(&self) -> &ActivePrompt {
        &self.submitted
    }

    pub fn active(&self) -> &ActivePrompt {
        &self.active
    }

    /// Explicit aliases for callers where `submitted`/`active` alone would be
    /// ambiguous at the call site.
    pub fn submitted_prompt(&self) -> &ActivePrompt {
        self.submitted()
    }

    pub fn active_operator_prompt(&self) -> &ActivePrompt {
        self.active()
    }

    /// In-memory parity for an explicitly ephemeral session. No store is
    /// touched, but the prompt still has a stable address and verified hashes
    /// for the lifetime of the context.
    pub fn ephemeral_operator(
        conversation_id: impl Into<String>,
        raw_text: impl Into<Vec<u8>>,
        model_text: impl Into<Vec<u8>>,
    ) -> Self {
        let id = PromptId::new();
        let receipt = PromptReceipt::new(
            id,
            conversation_id.into(),
            "ephemeral".into(),
            1,
            None,
            None,
            id,
            id,
            PromptOrigin::Operator,
            raw_text.into(),
            model_text.into(),
            0,
        );
        Self::new(receipt.clone(), receipt)
    }

    /// In-memory operator continuation of an existing objective. The new
    /// operator submission becomes active while retaining the parent's root.
    pub fn ephemeral_operator_continuation(
        conversation_id: impl Into<String>,
        raw_text: impl Into<Vec<u8>>,
        model_text: impl Into<Vec<u8>>,
        parent: &Self,
    ) -> anyhow::Result<Self> {
        let conversation_id = conversation_id.into();
        let parent_receipt = parent.submitted().receipt();
        parent_receipt.verify_integrity()?;
        if parent_receipt.conversation_id() != conversation_id {
            anyhow::bail!(
                "ephemeral operator continuation cannot cross from conversation `{}` to `{conversation_id}`",
                parent_receipt.conversation_id()
            );
        }
        let id = PromptId::new();
        let receipt = PromptReceipt::new(
            id,
            conversation_id,
            "ephemeral".into(),
            parent_receipt.seq().saturating_add(1),
            Some(parent_receipt.id()),
            Some(parent_receipt.id()),
            parent_receipt.root_prompt_id(),
            id,
            PromptOrigin::Operator,
            raw_text.into(),
            model_text.into(),
            parent_receipt.ts_claim().saturating_add(1),
        );
        Ok(Self::new(receipt.clone(), receipt))
    }

    /// In-memory harness retry explicitly descended from a prior ephemeral or
    /// durable context. The retry gets its own receipt, while operator
    /// authority remains the parent's validated active prompt.
    pub fn ephemeral_harness_retry(
        conversation_id: impl Into<String>,
        raw_text: impl Into<Vec<u8>>,
        model_text: impl Into<Vec<u8>>,
        parent: &Self,
    ) -> anyhow::Result<Self> {
        let conversation_id = conversation_id.into();
        let parent_receipt = parent.submitted().receipt();
        let active_receipt = parent.active().receipt().clone();
        parent_receipt.verify_integrity()?;
        active_receipt.verify_integrity()?;
        if parent_receipt.conversation_id() != conversation_id
            || active_receipt.conversation_id() != conversation_id
        {
            anyhow::bail!(
                "ephemeral harness retry cannot cross from conversation `{}` to `{conversation_id}`",
                parent_receipt.conversation_id()
            );
        }
        if active_receipt.origin() != PromptOrigin::Operator
            || active_receipt.active_operator_id() != Some(active_receipt.id())
            || active_receipt.root_prompt_id() != parent_receipt.root_prompt_id()
        {
            anyhow::bail!("ephemeral harness retry has no validated operator authority");
        }
        let receipt = PromptReceipt::new(
            PromptId::new(),
            conversation_id,
            "ephemeral".into(),
            parent_receipt.seq().saturating_add(1),
            Some(parent_receipt.id()),
            Some(parent_receipt.id()),
            active_receipt.root_prompt_id(),
            active_receipt.id(),
            PromptOrigin::HarnessRetry,
            raw_text.into(),
            model_text.into(),
            parent_receipt.ts_claim().saturating_add(1),
        );
        Ok(Self::new(receipt, active_receipt))
    }
}

fn digest_bytes(prefix: &[u8], bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix);
    hash_field(&mut hasher, bytes);
    hasher.finalize().to_hex().to_string()
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_optional_id(hasher: &mut blake3::Hasher, id: Option<PromptId>) {
    match id {
        Some(id) => {
            hasher.update(&[1]);
            hash_field(hasher, id.to_string().as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_id_renders_as_address_and_round_trips() {
        let id =
            PromptId::from_uuid(Uuid::parse_str("4a06770e-d04f-45f4-a10d-5fd4c51cbb07").unwrap());
        assert_eq!(
            id.to_string(),
            "prompt:4a06770e-d04f-45f4-a10d-5fd4c51cbb07"
        );
        assert_eq!(id.to_string().parse::<PromptId>().unwrap(), id);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"prompt:4a06770e-d04f-45f4-a10d-5fd4c51cbb07\""
        );
        assert_eq!(
            serde_json::from_str::<PromptId>("\"4a06770e-d04f-45f4-a10d-5fd4c51cbb07\"").unwrap(),
            id
        );
        assert_eq!(
            serde_json::from_str::<PromptId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
    }

    #[test]
    fn representation_digests_are_domain_separated() {
        let id = PromptId::new();
        let receipt = PromptReceipt::new(
            id,
            "conversation".into(),
            "writer".into(),
            1,
            None,
            None,
            id,
            id,
            PromptOrigin::Operator,
            b"same".to_vec(),
            b"same".to_vec(),
            7,
        );
        assert_ne!(receipt.raw_digest(), receipt.model_digest());
        receipt.verify_integrity().unwrap();
        receipt
            .into_legacy_v1_for_test()
            .verify_integrity()
            .unwrap();
    }

    #[test]
    fn ephemeral_retry_does_not_replace_operator_authority() {
        let operator = TurnPromptContext::ephemeral_operator("c", "raw", "operator");
        let retry =
            TurnPromptContext::ephemeral_harness_retry("c", "retry raw", "retry", &operator)
                .unwrap();
        assert_ne!(retry.submitted().id(), operator.submitted().id());
        assert_eq!(retry.active().id(), operator.active().id());
        assert_eq!(retry.submitted().root_prompt_id(), operator.active().id());
        retry.submitted().receipt().verify_integrity().unwrap();
    }

    #[test]
    fn ephemeral_retry_inherits_nearest_operator_not_objective_root() {
        let a = TurnPromptContext::ephemeral_operator("c", "A", "A");
        let b = TurnPromptContext::ephemeral_operator_continuation("c", "B", "B", &a).unwrap();
        let retry = TurnPromptContext::ephemeral_harness_retry("c", "retry", "retry", &b).unwrap();
        let retry_again =
            TurnPromptContext::ephemeral_harness_retry("c", "again", "again", &retry).unwrap();

        assert_eq!(b.submitted().root_prompt_id(), a.submitted().id());
        assert_eq!(retry.submitted().root_prompt_id(), a.submitted().id());
        assert_eq!(retry.active().id(), b.submitted().id());
        assert_eq!(retry_again.active().id(), b.submitted().id());
    }
}
