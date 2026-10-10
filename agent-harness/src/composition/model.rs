//! Bounded model catalog receipts and deferred proposals. No execution grants.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub cid: String,
    pub parked: bool,
    pub required: bool,
    pub bytes: usize,
    pub preview: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub catalog: ContentId,
    pub expected_head: Option<ContentId>,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub cards: Vec<Card>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Queued {
    pub catalog: ContentId,
    pub actor: Actor,
    pub proposal: Proposal,
}
impl ContentAddressable for Page {
    fn canonical_form(&self) -> std::result::Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
impl ContentAddressable for Queued {
    fn canonical_form(&self) -> std::result::Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
