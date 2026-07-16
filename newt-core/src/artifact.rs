//! Prompt-rooted, append-only records of work derived from an operator ask.
//!
//! Prompt artifacts are deliberately narrower than a second transcript. They
//! retain bounded internal state (plans, checkpoints, decisions, and outcomes)
//! or locators plus digests for external state (files and commits). Raw tool
//! streams do not belong here. The SQLite store supplies conversation/workspace
//! fencing and serializes each conversation into one tamper-evident chain.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

use crate::PromptId;

const ARTIFACT_HASH_V1_PREFIX: &[u8] = b"newt-prompt-artifact:v1";

/// Maximum UTF-8 bytes retained inline for an artifact's bounded body.
pub const MAX_ARTIFACT_BODY_BYTES: usize = 65_536;
/// Maximum serialized JSON bytes retained as structured artifact metadata.
pub const MAX_ARTIFACT_METADATA_BYTES: usize = 16_384;
/// Maximum UTF-8 bytes in a workspace-relative path or external locator.
pub const MAX_ARTIFACT_LOCATOR_BYTES: usize = 4_096;

/// Stable address of one immutable prompt artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactId(Uuid);

impl ArtifactId {
    /// Mint a new random artifact address.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Construct from an already-minted UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The UUID portion of this artifact address.
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "artifact:{}", self.0)
    }
}

impl FromStr for ArtifactId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix("artifact:").unwrap_or(value);
        Uuid::parse_str(value).map(Self)
    }
}

impl Serialize for ArtifactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// The bounded derived-work record being retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    PlanRevision,
    CompactionCheckpoint,
    FileChange,
    TurnOutcome,
    Commit,
    Decision,
}

impl ArtifactKind {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::PlanRevision => "plan_revision",
            Self::CompactionCheckpoint => "compaction_checkpoint",
            Self::FileChange => "file_change",
            Self::TurnOutcome => "turn_outcome",
            Self::Commit => "commit",
            Self::Decision => "decision",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "plan_revision" => Ok(Self::PlanRevision),
            "compaction_checkpoint" => Ok(Self::CompactionCheckpoint),
            "file_change" => Ok(Self::FileChange),
            "turn_outcome" => Ok(Self::TurnOutcome),
            "commit" => Ok(Self::Commit),
            "decision" => Ok(Self::Decision),
            other => anyhow::bail!("unknown prompt artifact kind `{other}`"),
        }
    }
}

/// Immutable link from a derived record to earlier work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRelation {
    DerivedFrom,
    Updates,
    Summarizes,
    Realizes,
}

impl ArtifactRelation {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::DerivedFrom => "derived_from",
            Self::Updates => "updates",
            Self::Summarizes => "summarizes",
            Self::Realizes => "realizes",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "derived_from" => Ok(Self::DerivedFrom),
            "updates" => Ok(Self::Updates),
            "summarizes" => Ok(Self::Summarizes),
            "realizes" => Ok(Self::Realizes),
            other => anyhow::bail!("unknown prompt artifact relation `{other}`"),
        }
    }
}

/// Content awaiting attachment to a durable prompt receipt.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPromptArtifact {
    kind: ArtifactKind,
    relation: ArtifactRelation,
    locator: Option<String>,
    body: Option<String>,
    metadata: Value,
}

impl NewPromptArtifact {
    /// Begin a bounded artifact with empty structured metadata.
    pub fn new(kind: ArtifactKind, relation: ArtifactRelation) -> Self {
        Self {
            kind,
            relation,
            locator: None,
            body: None,
            metadata: Value::Object(Map::new()),
        }
    }

    pub fn with_locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Attach structured metadata. Only JSON objects are accepted at append
    /// time, which keeps this field an index of facts rather than an escape
    /// hatch for a raw tool transcript.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    pub fn relation(&self) -> ArtifactRelation {
        self.relation
    }

    pub fn locator(&self) -> Option<&str> {
        self.locator.as_deref()
    }

    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub fn metadata(&self) -> &Value {
        &self.metadata
    }

    pub(crate) fn validate(&self) -> anyhow::Result<String> {
        if self
            .locator
            .as_ref()
            .is_some_and(|locator| locator.len() > MAX_ARTIFACT_LOCATOR_BYTES)
        {
            anyhow::bail!(
                "prompt artifact locator exceeds {MAX_ARTIFACT_LOCATOR_BYTES} UTF-8 bytes"
            );
        }
        if self
            .body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_ARTIFACT_BODY_BYTES)
        {
            anyhow::bail!("prompt artifact body exceeds {MAX_ARTIFACT_BODY_BYTES} UTF-8 bytes");
        }
        if !self.metadata.is_object() {
            anyhow::bail!("prompt artifact metadata must be a JSON object");
        }
        let metadata_json = serde_json::to_string(&self.metadata)?;
        if metadata_json.len() > MAX_ARTIFACT_METADATA_BYTES {
            anyhow::bail!(
                "prompt artifact metadata exceeds {MAX_ARTIFACT_METADATA_BYTES} serialized bytes"
            );
        }
        Ok(metadata_json)
    }
}

/// One immutable, prompt-rooted derived-work record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromptArtifact {
    id: ArtifactId,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    prev_hash: String,
    prompt_id: PromptId,
    root_prompt_id: PromptId,
    kind: ArtifactKind,
    relation: ArtifactRelation,
    locator: Option<String>,
    body: Option<String>,
    metadata: Value,
    #[serde(skip)]
    metadata_json: String,
    ts_claim: i64,
    encoding_version: i64,
    artifact_hash: String,
}

impl PromptArtifact {
    /// Mint a validated v1 artifact. Storage adapters supply the causal chain
    /// coordinates; hook code supplies only [`NewPromptArtifact`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mint(
        id: ArtifactId,
        conversation_id: String,
        writer_fingerprint: String,
        seq: i64,
        prev_hash: String,
        prompt_id: PromptId,
        root_prompt_id: PromptId,
        content: NewPromptArtifact,
        ts_claim: i64,
    ) -> anyhow::Result<Self> {
        if seq <= 0 {
            anyhow::bail!("prompt artifact sequence must be positive");
        }
        let metadata_json = content.validate()?;
        let mut artifact = Self {
            id,
            conversation_id,
            writer_fingerprint,
            seq,
            prev_hash,
            prompt_id,
            root_prompt_id,
            kind: content.kind,
            relation: content.relation,
            locator: content.locator,
            body: content.body,
            metadata: content.metadata,
            metadata_json,
            ts_claim,
            encoding_version: 1,
            artifact_hash: String::new(),
        };
        artifact.artifact_hash = artifact.compute_hash_v1();
        Ok(artifact)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_stored_parts(
        id: ArtifactId,
        conversation_id: String,
        writer_fingerprint: String,
        seq: i64,
        prev_hash: String,
        prompt_id: PromptId,
        root_prompt_id: PromptId,
        kind: ArtifactKind,
        relation: ArtifactRelation,
        locator: Option<String>,
        body: Option<String>,
        metadata_json: String,
        ts_claim: i64,
        encoding_version: i64,
        artifact_hash: String,
    ) -> anyhow::Result<Self> {
        let metadata: Value = serde_json::from_str(&metadata_json)?;
        let content = NewPromptArtifact {
            kind,
            relation,
            locator,
            body,
            metadata,
        };
        content.validate()?;
        Ok(Self {
            id,
            conversation_id,
            writer_fingerprint,
            seq,
            prev_hash,
            prompt_id,
            root_prompt_id,
            kind: content.kind,
            relation: content.relation,
            locator: content.locator,
            body: content.body,
            metadata: content.metadata,
            metadata_json,
            ts_claim,
            encoding_version,
            artifact_hash,
        })
    }

    pub fn id(&self) -> ArtifactId {
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

    pub fn prev_hash(&self) -> &str {
        &self.prev_hash
    }

    pub fn prompt_id(&self) -> PromptId {
        self.prompt_id
    }

    pub fn root_prompt_id(&self) -> PromptId {
        self.root_prompt_id
    }

    pub fn kind(&self) -> ArtifactKind {
        self.kind
    }

    pub fn relation(&self) -> ArtifactRelation {
        self.relation
    }

    pub fn locator(&self) -> Option<&str> {
        self.locator.as_deref()
    }

    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub fn metadata(&self) -> &Value {
        &self.metadata
    }

    pub fn ts_claim(&self) -> i64 {
        self.ts_claim
    }

    pub fn encoding_version(&self) -> i64 {
        self.encoding_version
    }

    pub fn artifact_hash(&self) -> &str {
        &self.artifact_hash
    }

    /// Verify bounded content and the immutable record hash.
    pub fn verify_integrity(&self) -> anyhow::Result<()> {
        NewPromptArtifact {
            kind: self.kind,
            relation: self.relation,
            locator: self.locator.clone(),
            body: self.body.clone(),
            metadata: self.metadata.clone(),
        }
        .validate()?;
        if self.metadata_json != serde_json::to_string(&self.metadata)? {
            anyhow::bail!("prompt artifact {} metadata encoding mismatch", self.id);
        }
        let expected = match self.encoding_version {
            1 => self.compute_hash_v1(),
            other => anyhow::bail!(
                "prompt artifact {} carries encoding_version {other}, which this newt does not understand",
                self.id
            ),
        };
        if expected != self.artifact_hash {
            anyhow::bail!("prompt artifact {} hash mismatch", self.id);
        }
        Ok(())
    }

    fn compute_hash_v1(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ARTIFACT_HASH_V1_PREFIX);
        for field in [
            self.id.to_string().as_bytes(),
            self.conversation_id.as_bytes(),
            self.writer_fingerprint.as_bytes(),
            self.prev_hash.as_bytes(),
            self.prompt_id.to_string().as_bytes(),
            self.root_prompt_id.to_string().as_bytes(),
            self.kind.as_db_str().as_bytes(),
            self.relation.as_db_str().as_bytes(),
        ] {
            hash_field(&mut hasher, field);
        }
        hasher.update(&self.seq.to_le_bytes());
        hash_optional_text(&mut hasher, self.locator.as_deref());
        hash_optional_text(&mut hasher, self.body.as_deref());
        hash_field(&mut hasher, self.metadata_json.as_bytes());
        hasher.update(&self.ts_claim.to_le_bytes());
        hasher.update(&self.encoding_version.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

fn hash_optional_text(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_field(hasher, value.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&(field.len() as u64).to_le_bytes());
    hasher.update(field);
}
