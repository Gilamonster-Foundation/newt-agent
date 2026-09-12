//! Append-only sessions. Every admitted object is durable before the journal names it.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use agent_frame::{
    Event, EventBody, EventKind, EventOrigin, Op, Packet, RawEvent, RawPacket, RootEvent, RootKind,
    Span, Unit,
};
use content_addressable::{ContentAddressable, ContentError, ContentId, MerkleNode, RawContentId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod tools;
pub(crate) use tools::ToolChange;
pub use tools::{ToolCallState, ToolCallStatus, ToolReturn};

use crate::{
    projection::{Entry, Projection},
    store::{FrameStore, RunWriter},
    Error, Result, Verdict,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub authority: String,
    pub authority_context: Option<Value>,
    pub max_catalog_entries: usize,
    pub max_fetched_bytes: usize,
    pub max_dereferences: usize,
    pub max_navigation_calls: usize,
    pub max_retries: usize,
    pub max_elapsed_ms: u64,
    pub max_history_nodes: usize,
    pub max_record_bytes: usize,
    pub max_slice_bytes: usize,
    pub auxiliary: Value,
    pub hermetic: bool,
    pub admitted_inputs: BTreeSet<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            authority: "local-session".into(),
            authority_context: None,
            max_catalog_entries: 64,
            max_fetched_bytes: 4 * 1024 * 1024,
            max_dereferences: 32,
            max_navigation_calls: 8,
            max_retries: 2,
            max_elapsed_ms: 30_000,
            max_history_nodes: 100_000,
            max_record_bytes: crate::store::DEFAULT_MAX_RECORD_BYTES,
            max_slice_bytes: 16_384,
            auxiliary: Value::Null,
            hermetic: false,
            admitted_inputs: BTreeSet::new(),
        }
    }
}

impl SessionConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(context) = &self.authority_context {
            let bytes =
                content_addressable::canonical::to_canonical_dagcbor(context).map_err(integrity)?;
            if ContentId::from_canonical_bytes(&bytes).to_string() != self.authority {
                return Err(Error::Access(
                    "authority context differs from its content identity".into(),
                ));
            }
        }
        if self.authority.is_empty()
            || self.max_catalog_entries == 0
            || self.max_fetched_bytes == 0
            || self.max_dereferences == 0
            || self.max_navigation_calls == 0
            || self.max_elapsed_ms == 0
            || self.max_history_nodes == 0
            || self.max_record_bytes == 0
            || self.max_slice_bytes == 0
        {
            return Err(Error::Access(
                "authority and all hard budgets must be nonempty".into(),
            ));
        }
        if self.hermetic && self.admitted_inputs.is_empty() {
            return Err(Error::Access(
                "hermetic sessions require an explicit admitted-input contract".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum JournalEntry {
    Run {
        schema: u32,
        config: SessionConfig,
        root: ContentId,
    },
    Observation {
        event: ContentId,
    },
    ToolOutput {
        event: ContentId,
        name: String,
    },
    ToolBatch {
        reply: ContentId,
        invocations: Vec<ContentId>,
        entries: Vec<ContentId>,
    },
    ToolCall {
        invocation: ContentId,
        change: ToolChange,
    },
    Reply {
        event: ContentId,
        request: ContentId,
    },
    ModelMessage {
        event: ContentId,
        reply: ContentId,
    },
    Verdict {
        event: ContentId,
        reply: ContentId,
        verdict: Verdict,
    },
    Failure {
        event: ContentId,
        reply: ContentId,
    },
    Intervention {
        event: ContentId,
    },
    Unit {
        unit: ContentId,
    },
    Packet {
        packet: ContentId,
        events: Vec<ContentId>,
    },
    Projection {
        projection: ContentId,
    },
    Request {
        request: ContentId,
    },
    Resume {
        starting: ContentId,
        authority: String,
    },
    Transcript {
        entries: Vec<ContentId>,
    },
    Outcome {
        event: ContentId,
        reply: ContentId,
        control: ControlOutcome,
    },
    Retrieval {
        receipt: ContentId,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlOutcome {
    Continue,
    ToolDispatch,
    AwaitOperator,
    Deliver,
    Incomplete,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetrievalPart {
    pub(crate) source: ContentId,
    pub(crate) span: Span,
    pub(crate) event: ContentId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetrievalBody {
    pub(crate) pointer: ContentId,
    pub(crate) span: Span,
    pub(crate) total: u64,
    pub(crate) payload: RawContentId,
    pub(crate) parts: Vec<RetrievalPart>,
}

struct Retrieved {
    bytes: Vec<u8>,
    total: usize,
    read_bytes: usize,
    parts: Vec<(ContentId, Span, Vec<u8>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestRecord {
    pub(crate) projection: ContentId,
    pub(crate) commitment: RawContentId,
    pub(crate) format: String,
}
impl ContentAddressable for RequestRecord {
    fn canonical_form(&self) -> std::result::Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

#[derive(Debug, Serialize)]
pub struct PreparedRequest {
    pub bytes: Vec<u8>,
    pub id: ContentId,
    pub projection: ContentId,
}

pub struct Session {
    store: FrameStore,
    writer: RunWriter,
    config: SessionConfig,
    head: ContentId,
    run: ContentId,
    root: ContentId,
    events: BTreeMap<ContentId, Event>,
    requests: BTreeSet<ContentId>,
    format: Option<String>,
    projections: BTreeSet<ContentId>,
    units: BTreeMap<ContentId, Unit>,
    packets: BTreeMap<ContentId, Packet>,
    packet_events: BTreeMap<ContentId, Vec<ContentId>>,
    reread_outputs: BTreeMap<String, Vec<ContentId>>,
    tool_calls: BTreeMap<ContentId, ToolCallStatus>,
    tool_order: Vec<ContentId>,
    pending: BTreeSet<ContentId>,
    replies: BTreeSet<ContentId>,
    model_messages: BTreeSet<ContentId>,
    verdicts: BTreeMap<ContentId, Verdict>,
    outcomes: BTreeMap<ContentId, ControlOutcome>,
    latest_reply: Option<ContentId>,
    seen: BTreeSet<ContentId>,
    transcript: Vec<ContentId>,
    restored_transcript: Vec<ContentId>,
    packet_head: Option<agent_frame::PacketId>,
    navigation_elapsed: Duration,
    fetched: usize,
    dereferences: usize,
    navigation_calls: usize,
    journal_len: usize,
    attempted_selections: BTreeSet<Vec<ContentId>>,
    aborted: bool,
}

impl Session {
    pub fn new(config: SessionConfig) -> Result<Self> {
        Self::create(
            FrameStore::memory_with_max_bytes(config.max_record_bytes)?,
            config,
        )
    }
    pub fn open(dir: impl AsRef<Path>, config: SessionConfig) -> Result<Self> {
        Self::create(
            FrameStore::open_with_max_bytes(dir, config.max_record_bytes)?,
            config,
        )
    }

    fn create(mut store: FrameStore, config: SessionConfig) -> Result<Self> {
        config.validate()?;
        // The root names this invocation, including an occurrence nonce. Two
        // independent processes with equal configuration must not share a head
        // locator. The nonce is content in the root, never an assigned node ID.
        let mut nonce = [0u8; 32];
        getrandom::getrandom(&mut nonce).map_err(|e| Error::Storage(e.to_string()))?;
        let bytes =
            serde_json::to_vec(&json!({"configuration":config,"invocation_nonce":nonce.to_vec()}))
                .map_err(integrity)?;
        store.put_source(&bytes)?;
        let root = store.put(&RootEvent::new(RootKind::HarnessEvent, &bytes, 0))?;
        let head = store.put(&MerkleNode::genesis(JournalEntry::Run {
            schema: 2,
            config: config.clone(),
            root,
        }))?;
        let writer = store.acquire_writer(head, None)?;
        store.publish_head(&writer, None, head)?;
        Ok(Self::empty(store, writer, config, head, root))
    }

    fn empty(
        store: FrameStore,
        writer: RunWriter,
        config: SessionConfig,
        head: ContentId,
        root: ContentId,
    ) -> Self {
        Self {
            store,
            writer,
            config,
            head,
            run: head,
            root,
            events: BTreeMap::new(),
            requests: BTreeSet::new(),
            format: None,
            projections: BTreeSet::new(),
            units: BTreeMap::new(),
            packets: BTreeMap::new(),
            packet_events: BTreeMap::new(),
            reread_outputs: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            tool_order: Vec::new(),
            pending: BTreeSet::new(),
            replies: BTreeSet::new(),
            model_messages: BTreeSet::new(),
            verdicts: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            latest_reply: None,
            seen: BTreeSet::new(),
            transcript: Vec::new(),
            restored_transcript: Vec::new(),
            packet_head: None,
            navigation_elapsed: Duration::ZERO,
            fetched: 0,
            dereferences: 0,
            navigation_calls: 0,
            journal_len: 1,
            attempted_selections: BTreeSet::new(),
            aborted: false,
        }
    }

    pub fn head(&self) -> ContentId {
        self.head
    }
    pub fn run_id(&self) -> ContentId {
        self.run
    }
    pub fn last_message(&self) -> Option<ContentId> {
        self.transcript.last().copied()
    }
    pub fn checkpoint_path(&self) -> Option<std::path::PathBuf> {
        self.store
            .directory()
            .map(|dir| dir.join("heads").join(self.run.to_string()))
    }
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }
    pub fn pending_replies(&self) -> Vec<ContentId> {
        self.pending.iter().copied().collect()
    }
    pub fn start_turn(&mut self) {
        self.transcript = self.restored_transcript.clone();
        self.navigation_elapsed = Duration::ZERO;
        self.fetched = 0;
        self.dereferences = 0;
        self.navigation_calls = 0;
        self.attempted_selections.clear();
    }

    /// Check execution ownership before host side effects. A failed check
    /// permanently stops this session; dropping it releases its writer lease.
    pub fn ensure_writer(&mut self) -> Result<()> {
        self.ensure_active()?;
        let result = self.store.check_writer(&self.writer, Some(self.head));
        if result.is_err() {
            self.aborted = true;
        }
        result
    }

    fn ensure_active(&self) -> Result<()> {
        if self.aborted {
            return Err(Error::Storage(
                "session aborted after a failed journal append; restore a verified checkpoint"
                    .into(),
            ));
        }
        Ok(())
    }

    fn append(&mut self, entry: JournalEntry) -> Result<()> {
        self.ensure_writer()?;
        let result = self.append_inner(entry);
        if result.is_err() {
            self.aborted = true;
        }
        result
    }

    fn append_inner(&mut self, entry: JournalEntry) -> Result<()> {
        if self.journal_len >= self.config.max_history_nodes {
            return Err(Error::Budget("session journal".into()));
        }
        let node = MerkleNode::new(entry, [self.head]);
        let head = self.store.put(&node)?;
        self.apply(node.payload())?;
        self.store
            .publish_head(&self.writer, Some(self.head), head)?;
        self.head = head;
        self.journal_len += 1;
        Ok(())
    }

    fn checked_event(&self, id: ContentId) -> Result<Event> {
        let raw: RawEvent = self.store.get(&id)?;
        let event = Event::admit(raw, |id| self.events.get(id).cloned()).map_err(integrity)?;
        if event.body().seq != self.journal_len as u64 {
            return Err(integrity(
                "event occurrence differs from its journal position",
            ));
        }
        let root: RootEvent = self.store.get(&event.body().root)?;
        self.store.source(&root.content)?;
        self.store.source(&event.body().payload)?;
        if event.body().origin == EventOrigin::Operator
            && event.body().kind == EventKind::Observation
        {
            if root.kind != RootKind::OperatorPrompt
                || root.content != event.body().payload
                || root.seq != event.body().seq
            {
                return Err(integrity("operator event root differs from its occurrence"));
            }
        } else if !matches!(
            event.body().kind,
            EventKind::Elision { .. } | EventKind::Retrieval
        ) && event.body().root != self.root
        {
            return Err(integrity("event names a different active root"));
        }
        if let EventKind::Elision { unit } = event.body().kind {
            let unit = self.unit(unit.into_content_id())?;
            let source_id = event
                .body()
                .sources
                .iter()
                .next()
                .ok_or_else(|| integrity("elision has no source"))?;
            let source = self
                .events
                .get(source_id)
                .ok_or_else(|| integrity("elision source absent"))?;
            event
                .verify_elision(
                    &unit,
                    source,
                    &self.store.source(&unit.derivation().source)?,
                )
                .map_err(integrity)?;
        }
        Ok(event)
    }

    fn apply(&mut self, entry: &JournalEntry) -> Result<()> {
        match entry {
            JournalEntry::Run { .. } => return Err(integrity("nested run record")),
            JournalEntry::ToolBatch {
                reply,
                invocations,
                entries,
            } => self.apply_tool_batch(*reply, invocations, entries)?,
            JournalEntry::ToolCall { invocation, change } => {
                self.apply_tool_change(*invocation, change)?;
            }
            JournalEntry::Observation { event } | JournalEntry::Intervention { event } => {
                let value = self.checked_event(*event)?;
                if !matches!(
                    (&entry, &value.body().kind),
                    (
                        JournalEntry::Observation { .. },
                        EventKind::Observation | EventKind::Elision { .. }
                    ) | (JournalEntry::Intervention { .. }, EventKind::Intervention)
                ) {
                    return Err(integrity("journal record relabels the event operation"));
                }
                if value.body().origin == EventOrigin::Operator
                    && value.body().kind == EventKind::Observation
                {
                    self.ensure_tools_closed()?;
                    self.root = value.body().root;
                }
                if value.body().kind == EventKind::Retrieval {
                    return Err(integrity("retrieval requires a verified range receipt"));
                }
                self.events.insert(*event, value);
            }
            JournalEntry::ToolOutput { event, name } => {
                let value = self.checked_event(*event)?;
                if name.is_empty()
                    || value.body().origin != EventOrigin::Tool
                    || value.body().kind != EventKind::Observation
                    || !self
                        .latest_reply
                        .into_iter()
                        .all(|reply| value.parents().contains(&reply))
                {
                    return Err(integrity(
                        "tool output lacks its name, origin, or invocation parent",
                    ));
                }
                self.events.insert(*event, value);
            }
            JournalEntry::Reply { event, request } => {
                if !self.requests.contains(request) {
                    return Err(integrity("reply names an unrecorded request"));
                }
                let value = self.checked_event(*event)?;
                if value.body().origin != EventOrigin::Model
                    || value.body().kind != EventKind::Observation
                {
                    return Err(integrity("reply is not a model observation"));
                }
                let record: RequestRecord = self.store.get(request)?;
                let projection: Projection = self.store.get(&record.projection)?;
                if !projection
                    .entries
                    .iter()
                    .all(|e| value.parents().contains(&e.event))
                {
                    return Err(integrity("reply omits dispatched antecedents"));
                }
                self.seen.extend(projection.entries.iter().map(|e| e.event));
                self.events.insert(*event, value);
                self.pending.insert(*event);
                self.replies.insert(*event);
                self.latest_reply = Some(*event);
            }
            JournalEntry::Verdict {
                event,
                reply,
                verdict,
            } => {
                if !self.pending.contains(reply) {
                    return Err(integrity("verdict requires an unadjudicated reply"));
                }
                let value = self.checked_event(*event)?;
                if value.body().kind != (EventKind::Verdict { verdict: *verdict })
                    || !value.body().sources.contains(reply)
                {
                    return Err(integrity("verdict names another observation"));
                }
                self.events.insert(*event, value);
                self.pending.remove(reply);
                self.verdicts.insert(*reply, *verdict);
            }
            JournalEntry::ModelMessage { event, reply } => {
                self.ensure_tools_closed()?;
                let value = self.checked_event(*event)?;
                let message: Value =
                    serde_json::from_slice(&self.store.source(&value.body().payload)?)
                        .map_err(integrity)?;
                if self.model_messages.contains(reply)
                    || !self.replies.contains(reply)
                    || value.body().origin != EventOrigin::Model
                    || value.body().kind != EventKind::Observation
                    || !value.parents().contains(reply)
                    || message["role"] != "assistant"
                    || !message["content"].is_string()
                {
                    return Err(integrity(
                        "model message lacks its observed reply or assistant text",
                    ));
                }
                self.events.insert(*event, value);
                self.model_messages.insert(*reply);
                self.transcript.push(*event);
                self.restored_transcript.push(*event);
            }
            JournalEntry::Failure { event, reply } => {
                if !self.pending.contains(reply) {
                    return Err(integrity("failure requires an unadjudicated reply"));
                }
                let value = self.checked_event(*event)?;
                if value.body().origin != EventOrigin::Harness || !value.parents().contains(reply) {
                    return Err(integrity("failure is not parented on its reply"));
                }
                self.events.insert(*event, value);
                self.pending.remove(reply);
            }
            JournalEntry::Unit { unit } => {
                let value = self.unit(*unit)?;
                self.units.insert(*unit, value);
            }
            JournalEntry::Packet { packet, events } => {
                let raw: RawPacket = self.store.get(packet)?;
                let value = Packet::try_from(raw).map_err(integrity)?;
                if value
                    .units()
                    .iter()
                    .any(|id| !self.units.contains_key(id.as_content_id()))
                    || value.prior() != self.packet_head
                    || events.len() != value.units().len()
                    || events.iter().zip(value.units()).any(|(id, unit)| {
                        self.events.get(id).is_none_or(|event| {
                            event.body().kind != (EventKind::Elision { unit: *unit })
                        })
                    })
                {
                    return Err(integrity("packet closure is incomplete"));
                }
                self.packet_head = Some((*packet).into());
                self.packet_events.insert(*packet, events.clone());
                self.packets.insert(*packet, value);
            }
            JournalEntry::Projection { projection } => {
                let value: Projection = self.store.get(projection)?;
                for entry in &value.entries {
                    let event = self
                        .events
                        .get(&entry.event)
                        .ok_or_else(|| integrity("projection names unadmitted event"))?;
                    if event.body().payload != entry.source {
                        return Err(integrity("projection substituted an event's material"));
                    }
                }
                value.render(&self.store)?;
                self.transcript = value.entries.iter().map(|e| e.event).collect();
                self.projections.insert(*projection);
            }
            JournalEntry::Request { request } => {
                self.ensure_tools_closed()?;
                let record: RequestRecord = self.store.get(request)?;
                if self
                    .format
                    .as_ref()
                    .is_some_and(|format| format != &record.format)
                {
                    return Err(Error::Access(
                        "provider format changed within the session".into(),
                    ));
                }
                if !self.projections.contains(&record.projection) {
                    return Err(integrity("request names an unrecorded projection"));
                }
                self.verify_request(&record)?;
                self.requests.insert(*request);
                self.format = Some(record.format);
            }
            JournalEntry::Resume {
                starting,
                authority,
            } => {
                if *starting != self.head
                    || *authority != self.config.authority
                    || self.config.hermetic
                {
                    return Err(Error::Access("resume contract mismatch".into()));
                }
            }
            JournalEntry::Transcript { entries } => {
                self.ensure_tools_closed()?;
                for id in entries {
                    let event = self
                        .events
                        .get(id)
                        .ok_or_else(|| integrity("transcript event is absent"))?;
                    let message: Value =
                        serde_json::from_slice(&self.store.source(&event.body().payload)?)
                            .map_err(integrity)?;
                    if message.get("role").and_then(Value::as_str).is_none()
                        && !matches!(
                            message.get("type").and_then(Value::as_str),
                            Some(
                                "function_call" | "function_call_output" | "reasoning" | "message"
                            )
                        )
                    {
                        return Err(integrity(
                            "transcript message has no admitted role or item type",
                        ));
                    }
                }
                self.restored_transcript = entries.clone();
            }
            JournalEntry::Outcome {
                event,
                reply,
                control,
            } => {
                if !self.replies.contains(reply)
                    || self.events.get(reply).is_none_or(|source| {
                        source.body().origin != EventOrigin::Model
                            || source.body().kind != EventKind::Observation
                    })
                {
                    return Err(integrity("control outcome requires a recorded model reply"));
                }
                if let Some(previous) = self.outcomes.get(reply) {
                    if !matches!(
                        previous,
                        ControlOutcome::Continue | ControlOutcome::ToolDispatch
                    ) || !matches!(
                        control,
                        ControlOutcome::Incomplete
                            | ControlOutcome::Cancelled
                            | ControlOutcome::Failed
                    ) {
                        return Err(integrity("reply already has a terminal control outcome"));
                    }
                }
                let value = self.checked_event(*event)?;
                if !value.body().sources.contains(reply)
                    || value.body().origin != EventOrigin::Harness
                {
                    return Err(integrity("control outcome has no model observation source"));
                }
                match control {
                    ControlOutcome::Deliver
                        if self.verdicts.get(reply) != Some(&Verdict::Answer) =>
                    {
                        return Err(integrity("delivery requires an admitted answer verdict"))
                    }
                    ControlOutcome::AwaitOperator
                        if self.verdicts.get(reply) != Some(&Verdict::Question) =>
                    {
                        return Err(integrity("awaiting operator requires a question verdict"))
                    }
                    ControlOutcome::ToolDispatch => {
                        return Err(integrity("tool dispatch requires a lifecycle batch"));
                    }
                    _ => {}
                }
                self.events.insert(*event, value);
                self.pending.remove(reply);
                self.outcomes.insert(*reply, *control);
            }
            JournalEntry::Retrieval { receipt } => {
                let node: MerkleNode<RetrievalBody> = self.store.get(receipt)?;
                let body = node.payload();
                if body.span.end < body.span.start
                    || body.span.end > body.total
                    || body.span.len() > self.config.max_slice_bytes as u64
                {
                    return Err(integrity(
                        "retrieval receipt declares an invalid or oversized range",
                    ));
                }
                let expected = self.retrieve_bytes(
                    body.pointer,
                    body.span.start as usize,
                    body.span.len() as usize,
                    self.config.max_fetched_bytes,
                    self.config.max_dereferences,
                )?;
                if body.total != expected.total as u64
                    || self.store.source(&body.payload)? != expected.bytes
                    || body.parts.len() != expected.parts.len()
                {
                    return Err(integrity("retrieval range or retained bytes differ"));
                }
                let parents = BTreeSet::from_iter(
                    std::iter::once(body.pointer).chain(body.parts.iter().map(|part| part.event)),
                );
                if node.parents() != &parents {
                    return Err(integrity(
                        "retrieval receipt parents differ from followed pointer",
                    ));
                }
                for (part, (source, span, bytes)) in body.parts.iter().zip(expected.parts) {
                    let event = self.checked_event(part.event)?;
                    if part.source != source
                        || part.span != span
                        || event.body().kind != EventKind::Retrieval
                        || event.body().sources != BTreeSet::from([source])
                        || self.store.source(&event.body().payload)? != bytes
                    {
                        return Err(integrity("retrieval artifact provenance differs"));
                    }
                    self.events.insert(part.event, event);
                }
                let output = self.retrieval_output(*receipt, body)?;
                self.reread_outputs.insert(
                    output.to_string(),
                    body.parts.iter().map(|part| part.event).collect(),
                );
            }
        }
        Ok(())
    }

    fn unit(&self, id: ContentId) -> Result<Unit> {
        self.store.unit(id)
    }

    /// Restore a verified closure before granting access to its events. Authority
    /// comes from the caller and must match; stored metadata cannot grant it.
    pub fn restore(dir: impl AsRef<Path>, head: ContentId, authority: &str) -> Result<Self> {
        Self::restore_checked(dir, head, authority, None)
    }

    pub fn restore_with_config(
        dir: impl AsRef<Path>,
        head: ContentId,
        config: &SessionConfig,
    ) -> Result<Self> {
        config.validate()?;
        Self::restore_checked(dir, head, &config.authority, Some(config))
    }

    fn restore_checked(
        dir: impl AsRef<Path>,
        head: ContentId,
        authority: &str,
        expected: Option<&SessionConfig>,
    ) -> Result<Self> {
        let store = FrameStore::open_with_max_bytes(
            dir,
            expected.map_or_else(
                || SessionConfig::default().max_record_bytes,
                |config| config.max_record_bytes,
            ),
        )?;
        let mut cursor = head;
        let mut chain = Vec::new();
        let mut visited = BTreeSet::new();
        let (config, root, genesis) = loop {
            if !visited.insert(cursor)
                || visited.len()
                    > expected.map_or_else(
                        || SessionConfig::default().max_history_nodes,
                        |config| config.max_history_nodes,
                    )
            {
                return Err(integrity("cyclic or oversized journal"));
            }
            let node: MerkleNode<JournalEntry> = store.get(&cursor)?;
            if let JournalEntry::Run {
                schema,
                config,
                root,
            } = node.payload()
            {
                config.validate()?;
                if *schema == 1 {
                    return Err(Error::Access("pre-lifecycle schema 1 sessions cannot prove per-call completion; inspect/replay the retained frame or start a new schema 2 run".into()));
                }
                if *schema != 2
                    || !node.parents().is_empty()
                    || config.authority != authority
                    || config.hermetic
                {
                    return Err(Error::Access(
                        "unsupported schema, authority, or hermetic resume".into(),
                    ));
                }
                if expected.is_some_and(|current| current != config) {
                    return Err(Error::Access(
                        "resume settings differ from the recorded run contract".into(),
                    ));
                }
                break (config.clone(), *root, cursor);
            }
            if node.parents().len() != 1 {
                return Err(integrity("session journal is not a chain"));
            }
            let prior = *node
                .parents()
                .iter()
                .next()
                .ok_or_else(|| integrity("missing predecessor"))?;
            chain.push((cursor, node));
            cursor = prior;
        };
        if chain.len() + 1 > config.max_history_nodes {
            return Err(Error::Budget("restoration journal".into()));
        }
        let event: RootEvent = store.get(&root)?;
        let origin: Value =
            serde_json::from_slice(&store.source(&event.content)?).map_err(integrity)?;
        if event.kind != RootKind::HarnessEvent
            || event.seq != 0
            || origin.get("configuration")
                != Some(&serde_json::to_value(&config).map_err(integrity)?)
            || origin
                .get("invocation_nonce")
                .and_then(Value::as_array)
                .is_none_or(|nonce| {
                    nonce.len() != 32
                        || nonce
                            .iter()
                            .any(|byte| byte.as_u64().is_none_or(|value| value > 255))
                })
        {
            return Err(integrity(
                "run root does not commit its configuration and occurrence",
            ));
        }
        let writer = store.acquire_writer(genesis, Some(head))?;
        let mut session = Self::empty(store, writer, config, genesis, root);
        for (id, node) in chain.into_iter().rev() {
            session.apply(node.payload())?;
            session.head = id;
            session.journal_len += 1;
        }
        session.append(JournalEntry::Resume {
            starting: head,
            authority: authority.into(),
        })?;
        session.interrupt_tool_batch("resumed after the previous writer stopped")?;
        Ok(session)
    }

    fn event(
        &mut self,
        origin: EventOrigin,
        kind: EventKind,
        payload: &[u8],
        parents: BTreeSet<ContentId>,
        sources: BTreeSet<ContentId>,
        depth: u32,
    ) -> Result<ContentId> {
        let source = self.store.put_source(payload)?;
        let root = if matches!(kind, EventKind::Elision { .. } | EventKind::Retrieval) {
            sources
                .iter()
                .next()
                .and_then(|id| self.events.get(id))
                .ok_or_else(|| integrity("derived source is absent"))?
                .body()
                .root
        } else if origin == EventOrigin::Operator && kind == EventKind::Observation {
            self.store.put(&RootEvent::new(
                RootKind::OperatorPrompt,
                payload,
                self.journal_len as u64,
            ))?
        } else {
            self.root
        };
        let event = Event::new(
            EventBody {
                root,
                origin,
                kind,
                payload: source,
                seq: self.journal_len as u64,
                sources,
                depth,
            },
            parents,
            |id| self.events.get(id).cloned(),
        )
        .map_err(integrity)?;
        self.store.put(&event)
    }

    fn ingest(&mut self, messages: &[Value]) -> Result<Vec<Entry>> {
        self.ensure_tools_closed()?;
        let mut entries = Vec::with_capacity(messages.len());
        let mut available: BTreeMap<RawContentId, std::collections::VecDeque<ContentId>> =
            BTreeMap::new();
        let mut seen = BTreeSet::new();
        for id in self.transcript.iter().chain(&self.restored_transcript) {
            if seen.insert(*id) {
                if let Some(event) = self.events.get(id) {
                    available
                        .entry(event.body().payload)
                        .or_default()
                        .push_back(*id);
                }
            }
        }
        for message in messages {
            let bytes = serde_json::to_vec(message).map_err(integrity)?;
            let source = RawContentId::from_content(&bytes);
            let old = available.get_mut(&source).and_then(|ids| ids.pop_front());
            let event = if let Some(id) = old {
                id
            } else {
                let role = crate::projection::role(message);
                let tool_contents = tool_contents(message);
                let retrieved = tool_contents
                    .iter()
                    .filter_map(|content| self.reread_outputs.get(*content))
                    .flatten()
                    .copied()
                    .collect::<BTreeSet<_>>();
                let origin = if !retrieved.is_empty() {
                    EventOrigin::Harness
                } else if !tool_contents.is_empty() {
                    EventOrigin::Tool
                } else {
                    match role {
                        "assistant" => EventOrigin::Model,
                        "tool" => EventOrigin::Tool,
                        "system" | "developer" => EventOrigin::Harness,
                        _ => EventOrigin::Operator,
                    }
                };
                let kind = if origin == EventOrigin::Harness {
                    EventKind::Intervention
                } else {
                    EventKind::Observation
                };
                let depth = if origin == EventOrigin::Harness { 1 } else { 0 };
                let mut parents = entries
                    .last()
                    .map(|e: &Entry| BTreeSet::from([e.event]))
                    .unwrap_or_default();
                parents.extend(retrieved);
                if matches!(origin, EventOrigin::Model | EventOrigin::Tool) {
                    parents.extend(self.latest_reply);
                }
                let id = self.event(origin, kind, &bytes, parents, BTreeSet::new(), depth)?;
                self.append(if origin == EventOrigin::Harness {
                    JournalEntry::Intervention { event: id }
                } else {
                    JournalEntry::Observation { event: id }
                })?;
                id
            };
            entries.push(Entry {
                event,
                source,
                role: crate::projection::role(message).into(),
                span: Span::new(0, bytes.len() as u64),
            });
        }
        self.transcript = entries.iter().map(|e| e.event).collect();
        Ok(entries)
    }

    pub fn record_messages(&mut self, messages: &[Value]) -> Result<()> {
        let entries = self
            .ingest(messages)?
            .iter()
            .map(|entry| entry.event)
            .collect();
        self.append(JournalEntry::Transcript { entries })
    }

    /// Retain an already-disclosed tool result before slicing or eliding it.
    /// The caller owns disclosure and tool authority; content IDs cannot grant either.
    pub fn retain_tool_output(&mut self, name: &str, bytes: &[u8]) -> Result<ContentId> {
        if name.is_empty() {
            return Err(integrity("tool output requires a name"));
        }
        let parents = self.latest_reply.into_iter().collect();
        let event = self.event(
            EventOrigin::Tool,
            EventKind::Observation,
            bytes,
            parents,
            BTreeSet::new(),
            0,
        )?;
        self.append(JournalEntry::ToolOutput {
            event,
            name: name.into(),
        })?;
        Ok(event)
    }

    pub fn restored_messages(&self) -> Result<Vec<Value>> {
        self.ensure_active()?;
        self.restored_transcript
            .iter()
            .map(|id| {
                let event = self
                    .events
                    .get(id)
                    .ok_or_else(|| integrity("restored message is not admitted"))?;
                serde_json::from_slice(&self.store.source(&event.body().payload)?)
                    .map_err(integrity)
            })
            .collect()
    }

    fn candidates(
        &self,
        messages: &[Value],
        entries: &[Entry],
    ) -> Vec<crate::navigation::Candidate> {
        let last_operator = messages.iter().zip(entries).rposition(|(message, entry)| {
            self.events[&entry.event].body().origin == EventOrigin::Operator
                && tool_pairs(message).is_empty()
        });
        let is_tool_result = |message: &Value| {
            crate::projection::role(message) == "tool"
                || message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().any(|v| v["type"] == "tool_result"))
        };
        // An observed server rejection does not make the newest result
        // disposable. Pair validation also retains its complete call group.
        let last_result = messages.iter().rposition(is_tool_result);
        messages
            .iter()
            .zip(entries)
            .enumerate()
            .map(|(index, (message, entry))| {
                let pairs = tool_pairs(message);
                let result = is_tool_result(message);
                crate::navigation::Candidate {
                    id: entry.event,
                    bytes: entry.span.len() as usize,
                    required: Some(index) == last_operator
                        || Some(index) == last_result
                        || matches!(entry.role.as_str(), "system" | "developer")
                        || (result && !self.seen.contains(&entry.event)),
                    pairs,
                }
            })
            .collect()
    }

    /// Bounded catalog cards contain source facts only. Harness interventions
    /// never become summarizer/relevance evidence.
    pub fn catalog(&mut self, messages: &[Value], max_bytes: usize) -> Result<Value> {
        self.navigation_work(|session| session.catalog_inner(messages, max_bytes))
    }

    fn catalog_inner(&mut self, messages: &[Value], max_bytes: usize) -> Result<Value> {
        self.check_navigation()?;
        let entries = self.ingest(messages)?;
        let candidates = self.candidates(messages, &entries);
        let start = entries
            .len()
            .saturating_sub(self.config.max_catalog_entries);
        let cards = entries
            .iter()
            .zip(&candidates)
            .enumerate()
            .filter(|(i, (_, c))| *i >= start || c.required)
            .filter(|(_, (entry, _))| {
                let event = &self.events[&entry.event];
                event.body().origin != EventOrigin::Harness && event.depth() == 0
            })
            .map(|(index, (entry, c))| {
                json!({
                    "cid":entry.event,"role":entry.role,"bytes":c.bytes,
                    "required":c.required,"pairs":c.pairs,
                    "excerpt":messages[index].get("content").and_then(Value::as_str)
                        .unwrap_or("").chars().take(160).collect::<String>()
                })
            })
            .collect::<Vec<_>>();
        if cards.len() > self.config.max_catalog_entries {
            return Err(Error::Budget("protected catalog entries".into()));
        }
        let pinned = entries
            .iter()
            .zip(&candidates)
            .filter(|(entry, c)| {
                c.required
                    && (self.events[&entry.event].body().origin == EventOrigin::Harness
                        || self.events[&entry.event].depth() > 0)
            })
            .map(|(entry, _)| entry.event)
            .collect::<Vec<_>>();
        Ok(json!({"max_bytes":max_bytes,"candidates":cards,"host_pinned":pinned}))
    }

    fn check_navigation(&mut self) -> Result<()> {
        if self.navigation_elapsed > Duration::from_millis(self.config.max_elapsed_ms)
            || self.navigation_calls >= self.config.max_navigation_calls
        {
            return Err(Error::Budget("navigation time or calls".into()));
        }
        self.navigation_calls += 1;
        Ok(())
    }

    /// A deterministic recent-context proposal. Legality uses the same validator
    /// as an auxiliary proposal; protected material cannot be repaired away.
    pub fn project(&mut self, messages: &[Value], max_bytes: usize) -> Result<Vec<Value>> {
        self.navigation_work(|session| session.project_inner(messages, max_bytes))
    }

    fn project_inner(&mut self, messages: &[Value], max_bytes: usize) -> Result<Vec<Value>> {
        self.check_navigation()?;
        let entries = self.ingest(messages)?;
        let candidates = self.candidates(messages, &entries);
        let all: Vec<_> = candidates.iter().map(|c| c.id).collect();
        if crate::navigation::validate_selection(&candidates, &all, max_bytes).is_ok() {
            return Ok(messages.to_vec());
        }
        let mut selected: BTreeSet<_> = candidates
            .iter()
            .filter(|c| c.required)
            .map(|c| c.id)
            .collect();
        // Tool exchanges form connected groups (one assistant message may call
        // several tools). Grow a complete group before checking its cost.
        complete_pairs(&candidates, &mut selected);
        for candidate in candidates
            .iter()
            .rev()
            .take(self.config.max_catalog_entries)
        {
            let mut proposal = selected.clone();
            proposal.insert(candidate.id);
            complete_pairs(&candidates, &mut proposal);
            if crate::navigation::validate_selection(
                &candidates,
                &proposal.iter().copied().collect::<Vec<_>>(),
                max_bytes.saturating_sub(512),
            )
            .is_ok()
            {
                selected = proposal;
            }
        }
        self.render_selection(
            messages,
            &entries,
            &candidates,
            &selected.iter().copied().collect::<Vec<_>>(),
            max_bytes,
        )
    }

    pub fn project_selection(
        &mut self,
        messages: &[Value],
        selected_cids: &[String],
        max_bytes: usize,
    ) -> Result<Vec<Value>> {
        self.navigation_work(|session| {
            session.project_selection_inner(messages, selected_cids, max_bytes)
        })
    }

    fn project_selection_inner(
        &mut self,
        messages: &[Value],
        selected_cids: &[String],
        max_bytes: usize,
    ) -> Result<Vec<Value>> {
        let entries = self.ingest(messages)?;
        let candidates = self.candidates(messages, &entries);
        let mut selected = selected_cids
            .iter()
            .map(|id| id.parse().map_err(integrity))
            .collect::<Result<Vec<_>>>()?;
        if self.attempted_selections.len() > self.config.max_retries
            || !self.attempted_selections.insert(selected.clone())
        {
            return Err(Error::Budget(
                "navigation retry or no-progress bound".into(),
            ));
        }
        // These entries are fixed by the host protocol, outside the model's
        // source-only relevance selection. The complete union is still validated.
        for candidate in &candidates {
            if candidate.required
                && (self.events[&candidate.id].body().origin == EventOrigin::Harness
                    || self.events[&candidate.id].depth() > 0)
                && !selected.contains(&candidate.id)
            {
                selected.push(candidate.id);
            }
        }
        self.render_selection(messages, &entries, &candidates, &selected, max_bytes)
    }

    pub fn record_navigation_request(
        &mut self,
        catalog: &Value,
        prompt: &str,
    ) -> Result<ContentId> {
        let sources = catalog
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or_else(|| integrity("catalog has no candidate array"))?
            .iter()
            .map(|v| {
                v.get("cid")
                    .and_then(Value::as_str)
                    .ok_or_else(|| integrity("catalog card has no CID"))?
                    .parse()
                    .map_err(integrity)
            })
            .collect::<Result<BTreeSet<ContentId>>>()?;
        for id in &sources {
            if self.events.get(id).is_none_or(|event| {
                event.body().origin == EventOrigin::Harness || event.depth() != 0
            }) {
                return Err(Error::Access(
                    "harness or generated material is not relevance evidence".into(),
                ));
            }
        }
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            prompt.as_bytes(),
            sources.clone(),
            sources,
            1,
        )?;
        self.append(JournalEntry::Intervention { event })?;
        Ok(event)
    }

    pub fn record_navigation_reply(&mut self, request: ContentId, reply: &str) -> Result<()> {
        let original = self
            .events
            .get(&request)
            .filter(|event| {
                event.body().origin == EventOrigin::Harness
                    && event.body().kind == EventKind::Intervention
            })
            .ok_or_else(|| integrity("navigation reply has no recorded request"))?;
        let sources = original.body().sources.clone();
        let mut parents = sources.clone();
        parents.insert(request);
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            reply.as_bytes(),
            parents,
            sources,
            1,
        )?;
        self.append(JournalEntry::Intervention { event })
    }

    pub fn record_navigation_failure(&mut self, request: ContentId, error: &str) -> Result<()> {
        self.record_navigation_reply(request, &json!({"failure":error}).to_string())
    }

    pub fn record_adjudication_request(
        &mut self,
        reply: ContentId,
        prompt: &str,
    ) -> Result<ContentId> {
        if !self.pending.contains(&reply) {
            return Err(integrity("adjudication requires an unadjudicated reply"));
        }
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            prompt.as_bytes(),
            BTreeSet::from([reply]),
            BTreeSet::from([reply]),
            1,
        )?;
        self.append(JournalEntry::Intervention { event })?;
        Ok(event)
    }

    pub fn record_adjudication_reply(&mut self, request: ContentId, reply: &str) -> Result<()> {
        self.record_navigation_reply(request, reply)
    }
    pub fn record_adjudication_failure(&mut self, request: ContentId, error: &str) -> Result<()> {
        self.record_navigation_failure(request, error)
    }

    pub fn record_outcome(
        &mut self,
        reply: ContentId,
        outcome: &str,
        delivered: &str,
    ) -> Result<()> {
        let control: ControlOutcome =
            serde_json::from_value(Value::String(outcome.into())).map_err(integrity)?;
        if control == ControlOutcome::ToolDispatch {
            return Err(integrity("tool dispatch requires normalized call records"));
        }
        let source = self
            .events
            .get(&reply)
            .ok_or_else(|| integrity("outcome observation is absent"))?;
        if source.body().origin != EventOrigin::Model
            || source.body().kind != EventKind::Observation
        {
            return Err(integrity("outcome must name a model observation"));
        }
        let payload = serde_json::to_vec(&json!({"role":"assistant","content":delivered}))
            .map_err(integrity)?;
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            &payload,
            BTreeSet::from([reply]),
            BTreeSet::from([reply]),
            1,
        )?;
        self.append(JournalEntry::Outcome {
            event,
            reply,
            control,
        })
    }

    fn render_selection(
        &mut self,
        messages: &[Value],
        entries: &[Entry],
        candidates: &[crate::navigation::Candidate],
        selected: &[ContentId],
        max_bytes: usize,
    ) -> Result<Vec<Value>> {
        crate::navigation::validate_selection(candidates, selected, max_bytes)?;
        let selection: BTreeSet<_> = selected.iter().copied().collect();
        let mut units = Vec::new();
        let mut events = Vec::new();
        for entry in entries
            .iter()
            .filter(|entry| !selection.contains(&entry.event))
        {
            let source_event = self.events[&entry.event].clone();
            if source_event.depth() != 0 {
                continue;
            }
            let bytes = self.store.source(&entry.source)?;
            let unit = Unit::seal(Op::Elide, &bytes, entry.span, source_event.body().root)
                .map_err(integrity)?;
            let id = self.store.put(&unit)?;
            self.append(JournalEntry::Unit { unit: id })?;
            let event = self.event(
                source_event.body().origin,
                EventKind::Elision { unit: id.into() },
                &bytes,
                BTreeSet::from([entry.event]),
                BTreeSet::from([entry.event]),
                0,
            )?;
            self.append(JournalEntry::Observation { event })?;
            units.push(id.into());
            events.push(event);
        }
        let mut projected = messages
            .iter()
            .zip(entries)
            .filter(|(_, entry)| selection.contains(&entry.event))
            .map(|(message, _)| message.clone())
            .collect::<Vec<_>>();
        let mut projected_events = entries
            .iter()
            .filter(|entry| selection.contains(&entry.event))
            .map(|entry| entry.event)
            .collect::<Vec<_>>();
        if !units.is_empty() {
            let packet = match self.packet_head {
                Some(prior) => Packet::following(prior, units),
                None => Packet::genesis(units),
            };
            let id = self.store.put(&packet)?;
            self.append(JournalEntry::Packet { packet: id, events })?;
            let text=format!("Earlier material is retained in frame {id}. Call re_read with that CID to retrieve bounded slices; a slice reports whether more remains.");
            let parent = entries
                .last()
                .ok_or_else(|| integrity("empty elision"))?
                .event;
            let index = projected
                .iter()
                .position(|m| !matches!(crate::projection::role(m), "system" | "developer"))
                .unwrap_or(projected.len());
            projected.insert(index, json!({"role":"user","content":text}));
            if serde_json::to_vec(&projected).map_err(integrity)?.len() > max_bytes {
                return Err(Error::Budget(
                    "protected inputs and re-read pointer do not fit".into(),
                ));
            }
            let pointer = self.record_host_message(&text, parent)?;
            projected_events.insert(index, pointer);
        }
        if serde_json::to_vec(&projected).map_err(integrity)?.len() > max_bytes {
            return Err(Error::Budget(
                "protected inputs and re-read pointer do not fit".into(),
            ));
        }
        self.transcript = projected_events;
        Ok(projected)
    }

    /// Retrieve only an admitted object from this session. The result names its
    /// actual byte interval and continuation; no truncated slice claims completion.
    pub fn re_read(&mut self, cid: &str, offset: usize, max_bytes: usize) -> Result<Value> {
        self.navigation_work(|session| session.re_read_inner(cid, offset, max_bytes))
    }

    /// Charge separately executed relevance work, including cancelled calls.
    /// Inference on the primary model and operator idle time are excluded.
    pub fn account_navigation_elapsed(&mut self, elapsed: Duration) {
        self.navigation_elapsed = self.navigation_elapsed.saturating_add(elapsed);
    }

    fn navigation_work<T>(&mut self, operation: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let limit = Duration::from_millis(self.config.max_elapsed_ms);
        if self.navigation_elapsed > limit {
            return Err(Error::Budget("navigation elapsed time".into()));
        }
        let started = Instant::now();
        let result = operation(self);
        self.account_navigation_elapsed(started.elapsed());
        if self.navigation_elapsed > limit {
            return Err(Error::Budget("navigation elapsed time".into()));
        }
        result
    }

    fn re_read_inner(&mut self, cid: &str, offset: usize, max_bytes: usize) -> Result<Value> {
        if max_bytes == 0
            || max_bytes > self.config.max_slice_bytes
            || self.dereferences >= self.config.max_dereferences
        {
            return Err(Error::Budget("retrieval bounds".into()));
        }
        let id: ContentId = cid.parse().map_err(integrity)?;
        let retrieved = self.retrieve_bytes(
            id,
            offset,
            max_bytes,
            self.config
                .max_fetched_bytes
                .checked_sub(self.fetched)
                .ok_or_else(|| Error::Budget("retrieval bytes".into()))?,
            self.config
                .max_dereferences
                .checked_sub(self.dereferences)
                .ok_or_else(|| Error::Budget("retrieval dereferences".into()))?,
        )?;
        self.fetched += retrieved.read_bytes;
        self.dereferences += retrieved.parts.len().max(1);
        let mut parts = Vec::new();
        for (source, span, bytes) in retrieved.parts {
            let original = self.events[&source].clone();
            let event = self.event(
                original.body().origin,
                EventKind::Retrieval,
                &bytes,
                BTreeSet::from([source]),
                BTreeSet::from([source]),
                original.depth(),
            )?;
            parts.push(RetrievalPart {
                source,
                span,
                event,
            });
        }
        let payload = self.store.put_source(&retrieved.bytes)?;
        let body = RetrievalBody {
            pointer: id,
            span: Span::new(offset as u64, (offset + retrieved.bytes.len()) as u64),
            total: retrieved.total as u64,
            payload,
            parts,
        };
        let parents = std::iter::once(id).chain(body.parts.iter().map(|part| part.event));
        let receipt = self.store.put(&MerkleNode::new(body.clone(), parents))?;
        self.append(JournalEntry::Retrieval { receipt })?;
        self.retrieval_output(receipt, &body)
    }

    fn retrieval_output(&self, receipt: ContentId, body: &RetrievalBody) -> Result<Value> {
        let bytes = self.store.source(&body.payload)?;
        Ok(
            json!({"cid":body.pointer,"retrieval":receipt,"bytes":bytes,"text":std::str::from_utf8(&bytes).ok(),"offset":body.span.start,"end":body.span.end,"total_bytes":body.total,"complete":body.span.end==body.total,"next_offset":if body.span.end<body.total {Some(body.span.end)}else{None}}),
        )
    }

    fn retrieve_bytes(
        &self,
        id: ContentId,
        offset: usize,
        max_bytes: usize,
        byte_budget: usize,
        deref_budget: usize,
    ) -> Result<Retrieved> {
        let mut result = Retrieved {
            bytes: Vec::new(),
            total: 0,
            read_bytes: 0,
            parts: Vec::new(),
        };
        if let Some(event) = self.events.get(&id) {
            result.total = self.store.source_len(&event.body().payload)?;
            if offset > result.total {
                return Err(Error::Proposal("retrieval offset exceeds material".into()));
            }
            if result.total > byte_budget || deref_budget == 0 {
                return Err(Error::Budget(
                    "retrieval source bytes or dereferences".into(),
                ));
            }
            let bytes = self.store.source(&event.body().payload)?;
            let end = offset.saturating_add(max_bytes).min(bytes.len());
            result.bytes = bytes[offset..end].to_vec();
            result.read_bytes = bytes.len();
            result.parts.push((
                id,
                Span::new(offset as u64, end as u64),
                result.bytes.clone(),
            ));
            return Ok(result);
        }
        let packet = self
            .packets
            .get(&id)
            .ok_or_else(|| Error::Access("CID absent from authorized session closure".into()))?;
        let mut total = 2usize;
        for (index, unit) in packet.units().iter().enumerate() {
            total = total
                .checked_add(
                    self.units[unit.as_content_id()].derivation().span.len() as usize
                        + usize::from(index > 0),
                )
                .ok_or_else(|| Error::Budget("packet size overflow".into()))?;
        }
        result.total = total;
        if offset > total {
            return Err(Error::Proposal("retrieval offset exceeds packet".into()));
        }
        let end = offset.saturating_add(max_bytes).min(total);
        append_overlap(&mut result.bytes, b"[", 0, offset, end);
        let mut position = 1;
        for (index, unit_id) in packet.units().iter().enumerate() {
            if index > 0 {
                append_overlap(&mut result.bytes, b",", position, offset, end);
                position += 1;
            }
            let unit = &self.units[unit_id.as_content_id()];
            let len = unit.derivation().span.len() as usize;
            let start = offset.max(position);
            let stop = end.min(position + len);
            if start < stop {
                if result.parts.len() >= deref_budget {
                    return Err(Error::Budget("retrieval dereferences".into()));
                }
                let read_len = self.store.source_len(&unit.derivation().source)?;
                if read_len > byte_budget.saturating_sub(result.read_bytes) {
                    return Err(Error::Budget("retrieval source bytes".into()));
                }
                let source = self.store.source(&unit.derivation().source)?;
                result.read_bytes += source.len();
                let bytes = unit
                    .derivation()
                    .span
                    .slice(&source)
                    .ok_or_else(|| integrity("elision source span absent"))?;
                let slice = bytes[start - position..stop - position].to_vec();
                result.bytes.extend_from_slice(&slice);
                let source_event = self.packet_events[&id][index];
                result.parts.push((
                    source_event,
                    Span::new((start - position) as u64, (stop - position) as u64),
                    slice,
                ));
            }
            position += len;
            if position >= end {
                break;
            }
        }
        append_overlap(&mut result.bytes, b"]", total - 1, offset, end);
        Ok(result)
    }

    pub fn record_request(&mut self, mut body: Value, format: &str) -> Result<PreparedRequest> {
        let field = if format == "responses" {
            "input"
        } else {
            "messages"
        };
        let object = body
            .as_object_mut()
            .ok_or_else(|| integrity("request must be an object"))?;
        let messages = match object.remove(field) {
            Some(Value::Array(messages)) => messages,
            Some(Value::String(content)) => vec![json!({"role":"user","content":content})],
            _ => return Err(integrity("request has no message input")),
        };
        self.prepare_request(body, format, field, &messages, "json-messages-v1")
    }

    /// Render original admitted messages through the shared provider renderer.
    /// Coalescing on the wire never relabels a host message as operator input.
    pub fn record_rendered_request(
        &mut self,
        body: Value,
        format: &str,
        messages: &[Value],
    ) -> Result<PreparedRequest> {
        if format != "anthropic" {
            return Err(integrity("unsupported provider renderer"));
        }
        self.prepare_request(body, format, "messages", messages, "anthropic-messages-v1")
    }

    fn prepare_request(
        &mut self,
        mut body: Value,
        format: &str,
        field: &str,
        messages: &[Value],
        renderer: &str,
    ) -> Result<PreparedRequest> {
        if self
            .format
            .as_ref()
            .is_some_and(|current| current != format)
        {
            return Err(Error::Access(
                "provider format changed within the session".into(),
            ));
        }
        let object = body
            .as_object_mut()
            .ok_or_else(|| integrity("request must be an object"))?;
        object.insert(field.into(), Value::Array(Vec::new()));
        let entries = self.ingest(messages)?;
        let template = self
            .store
            .put_source(&serde_json::to_vec(&body).map_err(integrity)?)?;
        let projection = Projection {
            schema: 1,
            renderer: renderer.into(),
            field: field.into(),
            template,
            entries,
        };
        let projection_id = self.store.put(&projection)?;
        self.append(JournalEntry::Projection {
            projection: projection_id,
        })?;
        let bytes = projection.render(&self.store)?;
        let commitment = self.store.put_source(&bytes)?;
        let id = self.store.put(&RequestRecord {
            projection: projection_id,
            commitment,
            format: format.into(),
        })?;
        self.append(JournalEntry::Request { request: id })?;
        Ok(PreparedRequest {
            bytes,
            id,
            projection: projection_id,
        })
    }

    fn verify_request(&self, record: &RequestRecord) -> Result<Vec<u8>> {
        crate::forensics::verify_request(&self.store, record)
    }

    pub fn replay(&self, request: ContentId) -> Result<Vec<u8>> {
        if !self.requests.contains(&request) {
            return Err(Error::Access("request is outside this session".into()));
        }
        self.verify_request(&self.store.get(&request)?)
    }

    pub fn record_reply(&mut self, request: ContentId, bytes: &[u8]) -> Result<ContentId> {
        if !self.requests.contains(&request) {
            return Err(Error::Access("reply has no admitted request".into()));
        }
        let record: RequestRecord = self.store.get(&request)?;
        let projection: Projection = self.store.get(&record.projection)?;
        let parents = projection.entries.iter().map(|e| e.event).collect();
        let event = self.event(
            EventOrigin::Model,
            EventKind::Observation,
            bytes,
            parents,
            BTreeSet::new(),
            0,
        )?;
        self.append(JournalEntry::Reply { event, request })?;
        Ok(event)
    }

    pub fn record_verdict(&mut self, reply: ContentId, verdict: Verdict) -> Result<()> {
        if !self.pending.contains(&reply) {
            return Err(integrity("verdict requires an unadjudicated observation"));
        }
        let payload = serde_json::to_vec(&verdict).map_err(integrity)?;
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Verdict { verdict },
            &payload,
            BTreeSet::from([reply]),
            BTreeSet::from([reply]),
            1,
        )?;
        self.append(JournalEntry::Verdict {
            event,
            reply,
            verdict,
        })
    }

    /// Record text extracted by the provider adapter from an observed reply.
    /// Host decorations belong in record_outcome, outside source evidence.
    pub fn record_model_message(&mut self, reply: ContentId, text: &str) -> Result<ContentId> {
        if !self.pending.contains(&reply) || self.model_messages.contains(&reply) {
            return Err(integrity("model text requires an unadjudicated reply"));
        }
        let payload =
            serde_json::to_vec(&json!({"role":"assistant","content":text})).map_err(integrity)?;
        let event = self.event(
            EventOrigin::Model,
            EventKind::Observation,
            &payload,
            BTreeSet::from([reply]),
            BTreeSet::new(),
            0,
        )?;
        self.append(JournalEntry::ModelMessage { event, reply })?;
        Ok(event)
    }

    pub fn record_failure(&mut self, reply: ContentId, error: &str) -> Result<()> {
        if !self.pending.contains(&reply) {
            return Err(integrity("failure requires an unadjudicated observation"));
        }
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            error.as_bytes(),
            BTreeSet::from([reply]),
            BTreeSet::new(),
            1,
        )?;
        self.append(JournalEntry::Failure { event, reply })
    }

    pub fn record_intervention(&mut self, text: &str, parent: ContentId) -> Result<ContentId> {
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            text.as_bytes(),
            BTreeSet::from([parent]),
            BTreeSet::new(),
            1,
        )?;
        self.append(JournalEntry::Intervention { event })?;
        Ok(event)
    }

    /// Explicitly admit a host-supplied wire message before inserting it in
    /// history. Arbitrary harness payloads never establish a user's origin.
    pub fn record_host_message(&mut self, text: &str, parent: ContentId) -> Result<ContentId> {
        self.record_host_envelope(&json!({"role":"user","content":text}), parent)
    }

    pub fn record_host_envelope(
        &mut self,
        message: &Value,
        parent: ContentId,
    ) -> Result<ContentId> {
        self.ensure_tools_closed()?;
        if !matches!(crate::projection::role(message), "user" | "tool") {
            return Err(integrity("host envelope must be a user or tool message"));
        }
        let payload = serde_json::to_vec(message).map_err(integrity)?;
        let event = self.event(
            EventOrigin::Harness,
            EventKind::Intervention,
            &payload,
            BTreeSet::from([parent]),
            BTreeSet::new(),
            1,
        )?;
        self.append(JournalEntry::Intervention { event })?;
        self.transcript.push(event);
        Ok(event)
    }
}

fn integrity(error: impl std::fmt::Display) -> Error {
    Error::Integrity(error.to_string())
}

fn append_overlap(target: &mut Vec<u8>, bytes: &[u8], position: usize, start: usize, end: usize) {
    let from = start.max(position);
    let to = end.min(position + bytes.len());
    if from < to {
        target.extend_from_slice(&bytes[from - position..to - position]);
    }
}

fn tool_contents(message: &Value) -> Vec<&str> {
    if crate::projection::role(message) == "tool" {
        return message
            .get("content")
            .or_else(|| message.get("output"))
            .and_then(Value::as_str)
            .into_iter()
            .collect();
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "tool_result")
        .filter_map(|item| item.get("content").and_then(Value::as_str))
        .collect()
}

fn tool_pairs(message: &Value) -> BTreeSet<String> {
    let mut pairs = BTreeSet::new();
    for key in ["tool_call_id", "call_id"] {
        if let Some(id) = message.get(key).and_then(Value::as_str) {
            pairs.insert(id.into());
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                pairs.insert(id.into());
            }
        }
    }
    if let Some(items) = message.get("content").and_then(Value::as_array) {
        for item in items {
            let key = if item["type"] == "tool_use" {
                "id"
            } else {
                "tool_use_id"
            };
            if let Some(id) = item.get(key).and_then(Value::as_str) {
                pairs.insert(id.into());
            }
        }
    }
    pairs
}

fn complete_pairs(candidates: &[crate::navigation::Candidate], selected: &mut BTreeSet<ContentId>) {
    loop {
        let before = selected.len();
        let pairs = candidates
            .iter()
            .filter(|c| selected.contains(&c.id))
            .flat_map(|c| c.pairs.iter())
            .collect::<BTreeSet<_>>();
        let additions = candidates
            .iter()
            .filter(|c| c.pairs.iter().any(|p| pairs.contains(p)))
            .map(|c| c.id)
            .collect::<Vec<_>>();
        selected.extend(additions);
        if selected.len() == before {
            break;
        }
    }
}

#[cfg(test)]
mod tests;
