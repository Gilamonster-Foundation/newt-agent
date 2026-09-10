//! Read-only reconstruction. Reading a record does not grant a session capability.
use crate::{projection::Projection, session::RequestRecord, store::FrameStore, Error, Result};
use content_addressable::{ContentId, RawContentId};

use crate::session::{JournalEntry, RetrievalBody, SessionConfig};
use agent_frame::{RawEvent, RootEvent};
use content_addressable::{canonical, MerkleNode, NodeStore};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Bounds cover the selected record and its immediate references together.
#[derive(Clone, Copy)]
pub struct InspectionLimits {
    pub max_bytes: usize,
    pub max_references: usize,
}

impl Default for InspectionLimits {
    fn default() -> Self {
        let config = SessionConfig::default();
        Self {
            max_bytes: config.max_fetched_bytes,
            max_references: config.max_catalog_entries,
        }
    }
}

#[derive(Serialize)]
pub struct Reference {
    pub relation: &'static str,
    pub cid: String,
    pub profile: &'static str,
}

/// An explanation is deliberately not an executable session capability.
#[derive(Serialize)]
pub struct Inspection {
    pub id: ContentId,
    pub kind: &'static str,
    pub parents: Vec<ContentId>,
    pub references: Vec<Reference>,
    pub record: Value,
    pub validation: &'static str,
    pub graph_admitted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Address {
    Node(ContentId),
    Raw(RawContentId),
}

/// Verify one addressed object and its immediate referenced objects. This does
/// not walk ancestry, validate the entire session, or grant stored authority.
/// `None` leaves existing Unit/Packet explanation to the kernel's own reports.
pub fn inspect_from_store(
    store: &FrameStore,
    id: ContentId,
    limits: InspectionLimits,
) -> Result<Option<Inspection>> {
    if limits.max_bytes == 0 || limits.max_references == 0 {
        return Err(Error::Budget("inspection bounds must be positive".into()));
    }
    let mut remaining = limits.max_bytes;
    let bytes = read_address(store, Address::Node(id), &mut remaining)?;
    let mut references = Vec::new();
    let (kind, parents, record) = if let Ok(node) =
        canonical::from_canonical_dagcbor_checked::<MerkleNode<JournalEntry>>(&bytes)
    {
        use JournalEntry::*;
        match node.payload() {
            Run { root, .. } => references.push(("root", Address::Node(*root))),
            Observation { event } | Intervention { event } | ToolOutput { event, .. } => {
                references.push(("event", Address::Node(*event)));
            }
            Reply { event, request } => references.extend([
                ("event", Address::Node(*event)),
                ("request", Address::Node(*request)),
            ]),
            Verdict { event, reply, .. }
            | Failure { event, reply }
            | Outcome { event, reply, .. }
            | ModelMessage { event, reply } => references.extend([
                ("event", Address::Node(*event)),
                ("reply", Address::Node(*reply)),
            ]),
            Unit { unit } => references.push(("unit", Address::Node(*unit))),
            Packet { packet, events } => {
                references.push(("packet", Address::Node(*packet)));
                references.extend(events.iter().map(|id| ("occurrence", Address::Node(*id))));
            }
            Projection { projection } => {
                references.push(("projection", Address::Node(*projection)));
            }
            Request { request } => references.push(("request", Address::Node(*request))),
            Resume { starting, .. } => references.push(("starting", Address::Node(*starting))),
            Transcript { entries } => {
                references.extend(entries.iter().map(|id| ("message", Address::Node(*id))));
            }
            Retrieval { receipt } => references.push(("receipt", Address::Node(*receipt))),
        }
        (
            "journal",
            node.parents().iter().copied().collect(),
            serde_json::to_value(node.payload()).map_err(invalid)?,
        )
    } else if let Ok(node) = canonical::from_canonical_dagcbor_checked::<RawEvent>(&bytes) {
        references.push(("root", Address::Node(node.payload().root)));
        references.push(("payload", Address::Raw(node.payload().payload)));
        references.extend(
            node.payload()
                .sources
                .iter()
                .map(|id| ("source", Address::Node(*id))),
        );
        if let agent_frame::EventKind::Elision { unit } = node.payload().kind {
            references.push(("unit", Address::Node(unit.into_content_id())));
        }
        (
            "event",
            node.parents().iter().copied().collect(),
            serde_json::to_value(node.payload()).map_err(invalid)?,
        )
    } else if let Ok(record) = canonical::from_canonical_dagcbor_checked::<RequestRecord>(&bytes) {
        references.extend([
            ("projection", Address::Node(record.projection)),
            ("commitment", Address::Raw(record.commitment)),
        ]);
        (
            "request",
            Vec::new(),
            serde_json::to_value(record).map_err(invalid)?,
        )
    } else if let Ok(record) = canonical::from_canonical_dagcbor_checked::<Projection>(&bytes) {
        references.push(("template", Address::Raw(record.template)));
        for entry in &record.entries {
            references.extend([
                ("event", Address::Node(entry.event)),
                ("source", Address::Raw(entry.source)),
            ]);
        }
        (
            "projection",
            Vec::new(),
            serde_json::to_value(record).map_err(invalid)?,
        )
    } else if let Ok(record) = canonical::from_canonical_dagcbor_checked::<RootEvent>(&bytes) {
        references.push(("source", Address::Raw(record.content)));
        (
            "root",
            Vec::new(),
            serde_json::to_value(record).map_err(invalid)?,
        )
    } else if let Ok(node) =
        canonical::from_canonical_dagcbor_checked::<MerkleNode<RetrievalBody>>(&bytes)
    {
        let record = serde_json::to_value(node.payload()).map_err(invalid)?;
        references.push((
            "pointer",
            Address::Node(serde_json::from_value(record["pointer"].clone()).map_err(invalid)?),
        ));
        references.push((
            "payload",
            Address::Raw(serde_json::from_value(record["payload"].clone()).map_err(invalid)?),
        ));
        if let Some(parts) = record["parts"].as_array() {
            for part in parts {
                for key in ["source", "event"] {
                    references.push((
                        key,
                        Address::Node(serde_json::from_value(part[key].clone()).map_err(invalid)?),
                    ));
                }
            }
        }
        (
            "retrieval",
            node.parents().iter().copied().collect(),
            record,
        )
    } else {
        return Ok(None);
    };
    references.extend(parents.iter().map(|id| ("parent", Address::Node(*id))));
    if references.len() > limits.max_references {
        return Err(Error::Budget("inspection references".into()));
    }
    let mut checked = BTreeSet::from([Address::Node(id)]);
    for (_, address) in &references {
        if checked.insert(*address) {
            read_address(store, *address, &mut remaining)?;
        }
    }
    let references = references
        .into_iter()
        .map(|(relation, address)| match address {
            Address::Node(id) => Reference {
                relation,
                cid: id.to_string(),
                profile: "dag-cbor",
            },
            Address::Raw(id) => Reference {
                relation,
                cid: id.to_string(),
                profile: "raw",
            },
        })
        .collect();
    Ok(Some(Inspection { id, kind, parents, references, record,
        validation: "canonical identity and immediate references only; full graph admission and authority are not established",
        graph_admitted: false,
    }))
}

fn read_address(store: &FrameStore, address: Address, remaining: &mut usize) -> Result<Vec<u8>> {
    let bytes = if let Some(dir) = store.directory() {
        let name = match address {
            Address::Node(id) => format!("{id}.cbor"),
            Address::Raw(id) => id.to_string(),
        };
        crate::store::read_bounded(&dir.join(name), (*remaining).min(store.max_record_bytes()))
            .map_err(invalid)?
    } else {
        match address {
            Address::Node(id) => store.get_unverified(&id).map_err(invalid)?,
            Address::Raw(id) => store.source(&id)?,
        }
    };
    if bytes.len() > *remaining {
        return Err(Error::Budget("inspection bytes".into()));
    }
    *remaining -= bytes.len();
    let matches = match address {
        Address::Node(id) => {
            ContentId::from_canonical_bytes_checked(&bytes).map_err(invalid)? == id
        }
        Address::Raw(id) => RawContentId::from_content(&bytes) == id,
    };
    if !matches {
        return Err(invalid("inspection reference content was substituted"));
    }
    Ok(bytes)
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Integrity(error.to_string())
}

pub fn replay_from_store(store: &FrameStore, request: ContentId) -> Result<Vec<u8>> {
    verify_request(store, &store.get(&request)?)
}

pub(crate) fn verify_request(store: &FrameStore, record: &RequestRecord) -> Result<Vec<u8>> {
    let projection: Projection = store.get(&record.projection)?;
    let bytes = projection.render(store)?;
    if RawContentId::from_content(&bytes) != record.commitment
        || store.source(&record.commitment)? != bytes
    {
        return Err(Error::Integrity(
            "request replay differs from dispatch commitment".into(),
        ));
    }
    Ok(bytes)
}
