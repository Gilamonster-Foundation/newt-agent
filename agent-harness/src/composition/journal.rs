//! Typed host API records. Reasons and actor labels are claims, not evidence.
use super::Pins;
use crate::projection::Entry;
use content_addressable::{ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub model: String,
    pub harness: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Complete serialized provider request, including its template.
    pub max_bytes: usize,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    Include,
    Park,
    Summarise,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub occurrence: ContentId,
    pub action: Action,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub expected_head: Option<ContentId>,
    pub changes: Vec<Change>,
    /// Restore this accepted decision's before projection; never rewind history.
    pub inverse: Option<ContentId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offered: Option<ContentId>,
    pub run: ContentId,
    pub root: ContentId,
    pub request: ContentId,
    pub expected_head: Option<ContentId>,
    pub before: ContentId,
    pub universe: ContentId,
    pub policy: ContentId,
    pub context_revision: Option<ContentId>,
    pub pin_revision: Option<ContentId>,
    pub pins: Option<ContentId>,
    /// Ordered occurrences, including parked material. Not a model-facing wire.
    pub entries: Vec<Entry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Submission {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<ContentId>,
    pub catalog: ContentId,
    pub actor: ContentId,
    pub proposal: Proposal,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Refusal {
    Stale,
    InvalidChange,
    RequiredInput,
    Capacity,
    NoProgress,
    Unsupported,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Refused(Refusal),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Elision {
    pub occurrence: ContentId,
    pub unit: ContentId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<ContentId>,
    pub version: u32,
    pub run: ContentId,
    pub root: ContentId,
    pub expected_head: Option<ContentId>,
    pub catalog: ContentId,
    pub proposal: ContentId,
    pub actor: ContentId,
    pub objective_ref: Option<String>,
    pub context_revision: Option<ContentId>,
    pub pin_revision: Option<ContentId>,
    pub pins: Option<ContentId>,
    pub policy: ContentId,
    pub before: ContentId,
    pub after: Option<ContentId>,
    pub outcome: Outcome,
    pub elisions: Vec<Elision>,
    pub bytes: usize,
}
#[derive(Debug, Clone)]
pub struct Receipt {
    pub decision: ContentId,
    pub outcome: Outcome,
    pub projection: ContentId,
}
#[derive(Default)]
pub(crate) struct State {
    pub pending: Vec<ContentId>,
    pub latest_catalog: Option<ContentId>,
    pub attempts: usize,
    pub pages: BTreeMap<ContentId, BTreeSet<ContentId>>,
    pub head: Option<ContentId>,
    pub active: Option<ContentId>,
    pub context_revision: Option<ContentId>,
    pub pin_revision: Option<ContentId>,
    pub latest_request: Option<ContentId>,
    pub catalogs: BTreeSet<ContentId>,
    pub proposals: BTreeSet<ContentId>,
    pub decisions: BTreeMap<ContentId, Decision>,
}

macro_rules! addressed {
    ($($t:ty),* $(,)?) => {$(impl ContentAddressable for $t {
        fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
            content_addressable::canonical::to_canonical_dagcbor(self)
        }
    })*};
}
addressed!(Actor, Policy, Catalog, Submission, Decision);

pub(crate) fn pin_identity(pins: &Option<Pins>) -> crate::Result<Option<ContentId>> {
    pins.as_ref()
        .map(|p| {
            p.content_id()
                .map_err(|e| crate::Error::Integrity(e.to_string()))
        })
        .transpose()
}
