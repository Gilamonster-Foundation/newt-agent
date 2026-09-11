//! Tool occurrence facts in the session's existing verified journal.
use super::*;

/// A return observed by the host. String contents never establish success or
/// failure. Callers own disclosure, tool authority, and the truth of this claim.
pub enum ToolReturn<'a> {
    Observed {
        bytes: &'a [u8],
        retained_sources: &'a [ContentId],
    },
    /// A typed error actually received from the tool, not a frame storage error.
    /// This does not assert that the failed operation had no side effects.
    Failed {
        bytes: &'a [u8],
        retained_sources: &'a [ContentId],
    },
    /// The exact serialized result of this session's admitted `re_read`.
    Retrieval(&'a str),
    /// A known host refusal or other host-authored return.
    Host(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Queued,
    Started,
    Returned,
    Failed,
    NotStarted,
    Uncertain,
    HostResolved,
}

/// Read-only derived state; its identity names the admitted invocation event.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallStatus {
    pub id: ContentId,
    pub reply: ContentId,
    pub ordinal: usize,
    pub call: Value,
    pub state: ToolCallState,
    /// The exact observed return, including a typed failure when present.
    pub returned: Option<ContentId>,
    pub retained_sources: Vec<ContentId>,
    pub delivery: Option<ContentId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReturnKind {
    Observed,
    Failed,
    Retrieval,
    Host,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ToolChange {
    Started,
    Returned {
        event: ContentId,
        sources: Vec<ContentId>,
        kind: ReturnKind,
    },
    Sources {
        sources: Vec<ContentId>,
    },
    Delivered {
        event: ContentId,
    },
    Resolved {
        event: ContentId,
    },
    Closed {
        event: ContentId,
        reason: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Occurrence {
    ordinal: usize,
    call: Value,
}

impl Session {
    pub fn tool_call(&self, id: ContentId) -> Result<&ToolCallStatus> {
        self.ensure_active()?;
        self.tool_calls
            .get(&id)
            .ok_or_else(|| integrity("tool invocation is not admitted"))
    }

    pub(super) fn ensure_tools_closed(&self) -> Result<()> {
        self.ensure_active()?;
        if self.tool_calls.values().any(|call| call.delivery.is_none()) {
            return Err(integrity("tool batch is incomplete; record delivery or interrupt_tool_batch before continuation"));
        }
        Ok(())
    }

    /// Publish accepted calls and the original assistant envelope atomically
    /// before any call starts. Wire decoding and external authenticity belong
    /// to the host; the leaf verifies normalized calls and their envelope match.
    pub fn begin_tool_batch(
        &mut self,
        reply: ContentId,
        calls: &[Value],
        messages: &[Value],
    ) -> Result<Vec<ContentId>> {
        self.ensure_tools_closed()?;
        self.ensure_writer()?;
        if !self.pending.contains(&reply) || self.latest_reply != Some(reply) {
            return Err(integrity(
                "tool batch requires the current unadjudicated reply",
            ));
        }
        self.validate_batch_messages(calls, messages)?;
        let entries = self
            .ingest(messages)?
            .iter()
            .map(|entry| entry.event)
            .collect();
        let mut invocations = Vec::with_capacity(calls.len());
        for (ordinal, call) in calls.iter().enumerate() {
            let payload = serde_json::to_vec(&Occurrence {
                ordinal,
                call: call.clone(),
            })
            .map_err(integrity)?;
            invocations.push(self.event(
                EventOrigin::Harness,
                EventKind::Intervention,
                &payload,
                BTreeSet::from([reply]),
                BTreeSet::from([reply]),
                1,
            )?);
        }
        self.append(JournalEntry::ToolBatch {
            reply,
            invocations: invocations.clone(),
            entries,
        })?;
        Ok(invocations)
    }

    pub fn start_tool_call(&mut self, id: ContentId) -> Result<()> {
        if self.tool_call(id)?.state != ToolCallState::Queued {
            return Err(integrity("only a queued tool call can start"));
        }
        self.append(JournalEntry::ToolCall {
            invocation: id,
            change: ToolChange::Started,
        })
    }

    /// Retain the return before presentation, warnings, or another tool. The
    /// additional sources must already be retained, admitted external outputs.
    /// An I/O error is propagated and never converted into a Failed tool fact.
    pub fn record_tool_return(
        &mut self,
        id: ContentId,
        returned: ToolReturn<'_>,
    ) -> Result<ContentId> {
        let call = self.tool_call(id)?.clone();
        if call.state != ToolCallState::Started {
            return Err(integrity("tool return requires a started invocation"));
        }
        let (bytes, sources, kind) = match returned {
            ToolReturn::Observed {
                bytes,
                retained_sources,
            } => (bytes, retained_sources, ReturnKind::Observed),
            ToolReturn::Failed {
                bytes,
                retained_sources,
            } => (bytes, retained_sources, ReturnKind::Failed),
            ToolReturn::Retrieval(text) => (text.as_bytes(), &[][..], ReturnKind::Retrieval),
            ToolReturn::Host(text) => (text.as_bytes(), &[][..], ReturnKind::Host),
        };
        let (origin, parents) = self.return_provenance(&call, bytes, sources, kind)?;
        self.ensure_writer()?;
        let result = (|| {
            let event = self.event(
                origin,
                if origin == EventOrigin::Tool {
                    EventKind::Observation
                } else {
                    EventKind::Intervention
                },
                bytes,
                parents,
                BTreeSet::new(),
                u32::from(origin == EventOrigin::Harness),
            )?;
            self.append(JournalEntry::ToolCall {
                invocation: id,
                change: ToolChange::Returned {
                    event,
                    sources: sources.to_vec(),
                    kind,
                },
            })?;
            Ok(event)
        })();
        if result.is_err() {
            self.aborted = true;
        }
        result
    }

    /// Attach verified external sources after the raw return is durable. This
    /// preserves an observed return if resolving a later spill hint fails. The
    /// attachment is a separate journal fact; it never changes the return CID.
    /// Sources must belong to this batch and may be attached only once, before
    /// delivery. An empty list validates the call without publishing a record.
    pub fn record_tool_sources(&mut self, id: ContentId, sources: &[ContentId]) -> Result<()> {
        self.validate_source_attachment(self.tool_call(id)?, sources)?;
        if sources.is_empty() {
            return Ok(());
        }
        self.append(JournalEntry::ToolCall {
            invocation: id,
            change: ToolChange::Sources {
                sources: sources.to_vec(),
            },
        })
    }

    /// Attach the exact existing provider envelope. Changed presentation is a
    /// harness projection of the observed return, never a new Tool observation.
    pub fn record_tool_delivery(&mut self, id: ContentId, envelope: &Value) -> Result<()> {
        let call = self.tool_call(id)?.clone();
        self.validate_delivery_slot(&call, envelope)?;
        let (origin, kind, sources, depth) = self.delivery_provenance(&call, envelope)?;
        let mut parents = BTreeSet::from([id]);
        parents.extend(call.returned);
        self.write_delivery(
            &call,
            envelope,
            origin,
            kind,
            parents,
            sources,
            depth,
            |event| ToolChange::Delivered { event },
        )
    }

    /// Resolve a queued call without executing it. This is explicitly a host
    /// substitute and cannot be mistaken for an observed tool return.
    pub fn resolve_tool_call(&mut self, id: ContentId, envelope: &Value) -> Result<()> {
        let call = self.tool_call(id)?.clone();
        self.validate_delivery_slot(&call, envelope)?;
        if call.state != ToolCallState::Queued {
            return Err(integrity("host resolution requires a queued invocation"));
        }
        self.write_delivery(
            &call,
            envelope,
            EventOrigin::Harness,
            EventKind::Intervention,
            BTreeSet::from([id]),
            BTreeSet::new(),
            1,
            |event| ToolChange::Resolved { event },
        )
    }

    /// Close protocol slots in original order without rerunning any tool.
    /// Returned/Failed facts survive missing presentation; their host notice
    /// points to retained bytes. No retrieval budget is spent on these pointers.
    /// With no unfinished slots this performs no writes.
    pub fn interrupt_tool_batch(&mut self, reason: &str) -> Result<Vec<Value>> {
        self.ensure_active()?;
        let pending = self
            .tool_order
            .iter()
            .copied()
            .filter(|id| self.tool_calls[id].delivery.is_none())
            .collect::<Vec<_>>();
        for id in pending {
            let call = self.tool_call(id)?.clone();
            let envelope = self.repair_envelope(&call, reason)?;
            self.validate_delivery_slot(&call, &envelope)?;
            let (parents, sources) = self.repair_provenance(&call);
            self.write_delivery(
                &call,
                &envelope,
                EventOrigin::Harness,
                EventKind::Intervention,
                parents,
                sources,
                1,
                |event| ToolChange::Closed {
                    event,
                    reason: reason.into(),
                },
            )?;
        }
        self.restored_messages()
    }

    #[allow(clippy::too_many_arguments)]
    fn write_delivery(
        &mut self,
        call: &ToolCallStatus,
        envelope: &Value,
        origin: EventOrigin,
        kind: EventKind,
        parents: BTreeSet<ContentId>,
        sources: BTreeSet<ContentId>,
        depth: u32,
        change: impl FnOnce(ContentId) -> ToolChange,
    ) -> Result<()> {
        self.ensure_writer()?;
        let result = (|| {
            let bytes = serde_json::to_vec(envelope).map_err(integrity)?;
            let event = self.event(origin, kind, &bytes, parents, sources, depth)?;
            self.append(JournalEntry::ToolCall {
                invocation: call.id,
                change: change(event),
            })
        })();
        if result.is_err() {
            self.aborted = true;
        }
        result
    }

    pub(super) fn apply_tool_batch(
        &mut self,
        reply: ContentId,
        invocations: &[ContentId],
        entries: &[ContentId],
    ) -> Result<()> {
        self.ensure_tools_closed()?;
        if !self.pending.contains(&reply)
            || self.latest_reply != Some(reply)
            || invocations.is_empty()
        {
            return Err(integrity(
                "tool batch lacks its current pending reply or calls",
            ));
        }
        let messages = entries
            .iter()
            .map(|id| {
                let event = self
                    .events
                    .get(id)
                    .ok_or_else(|| integrity("batch transcript event is absent"))?;
                serde_json::from_slice(&self.store.source(&event.body().payload)?)
                    .map_err(integrity)
            })
            .collect::<Result<Vec<Value>>>()?;
        let mut admitted = Vec::with_capacity(invocations.len());
        for (ordinal, id) in invocations.iter().enumerate() {
            let event = self.checked_event(*id)?;
            let occurrence: Occurrence =
                serde_json::from_slice(&self.store.source(&event.body().payload)?)
                    .map_err(integrity)?;
            if occurrence.ordinal != ordinal
                || self.events.contains_key(id)
                || event.body().origin != EventOrigin::Harness
                || event.body().kind != EventKind::Intervention
                || event.body().sources != BTreeSet::from([reply])
                || event.parents() != &BTreeSet::from([reply])
            {
                return Err(integrity(
                    "tool invocation identity, occurrence, or reply provenance differs",
                ));
            }
            admitted.push((event, occurrence));
        }
        self.validate_batch_messages(
            &admitted
                .iter()
                .map(|(_, call)| call.call.clone())
                .collect::<Vec<_>>(),
            &messages,
        )?;
        for (id, (event, occurrence)) in invocations.iter().zip(admitted) {
            self.events.insert(*id, event);
            self.tool_calls.insert(
                *id,
                ToolCallStatus {
                    id: *id,
                    reply,
                    ordinal: occurrence.ordinal,
                    call: occurrence.call,
                    state: ToolCallState::Queued,
                    returned: None,
                    retained_sources: Vec::new(),
                    delivery: None,
                },
            );
            self.tool_order.push(*id);
        }
        self.pending.remove(&reply);
        self.outcomes.insert(reply, ControlOutcome::ToolDispatch);
        self.transcript = entries.to_vec();
        self.restored_transcript = entries.to_vec();
        Ok(())
    }

    pub(super) fn apply_tool_change(&mut self, id: ContentId, change: &ToolChange) -> Result<()> {
        let mut call = self.tool_call(id)?.clone();
        match change {
            ToolChange::Started => {
                if call.state != ToolCallState::Queued {
                    return Err(integrity("only queued calls can start"));
                }
                call.state = ToolCallState::Started;
            }
            ToolChange::Returned {
                event,
                sources,
                kind,
            } => {
                if call.state != ToolCallState::Started {
                    return Err(integrity("only started calls can return"));
                }
                let value = self.checked_event(*event)?;
                let bytes = self.store.source(&value.body().payload)?;
                let (origin, parents) = self.return_provenance(&call, &bytes, sources, *kind)?;
                let expected_kind = if origin == EventOrigin::Tool {
                    EventKind::Observation
                } else {
                    EventKind::Intervention
                };
                if value.body().origin != origin
                    || value.body().kind != expected_kind
                    || !value.body().sources.is_empty()
                    || value.parents() != &parents
                {
                    return Err(integrity(
                        "tool return provenance differs from its admitted producer",
                    ));
                }
                self.events.insert(*event, value);
                call.returned = Some(*event);
                call.retained_sources = sources.clone();
                call.state = if matches!(kind, ReturnKind::Failed) {
                    ToolCallState::Failed
                } else {
                    ToolCallState::Returned
                };
            }
            ToolChange::Sources { sources } => {
                self.validate_source_attachment(&call, sources)?;
                if sources.is_empty() {
                    return Err(integrity(
                        "empty tool source attachment is not a journal fact",
                    ));
                }
                call.retained_sources.extend(sources);
            }
            ToolChange::Delivered { event }
            | ToolChange::Resolved { event }
            | ToolChange::Closed { event, .. } => {
                let value = self.checked_event(*event)?;
                let envelope: Value =
                    serde_json::from_slice(&self.store.source(&value.body().payload)?)
                        .map_err(integrity)?;
                self.validate_delivery_slot(&call, &envelope)?;
                let (origin, kind, parents, sources, depth) = match change {
                    ToolChange::Delivered { .. } => {
                        let (origin, kind, sources, depth) =
                            self.delivery_provenance(&call, &envelope)?;
                        let mut parents = BTreeSet::from([id]);
                        parents.extend(call.returned);
                        (origin, kind, parents, sources, depth)
                    }
                    ToolChange::Resolved { .. } => {
                        if call.state != ToolCallState::Queued {
                            return Err(integrity("host resolution requires a queued call"));
                        }
                        call.state = ToolCallState::HostResolved;
                        (
                            EventOrigin::Harness,
                            EventKind::Intervention,
                            BTreeSet::from([id]),
                            BTreeSet::new(),
                            1,
                        )
                    }
                    ToolChange::Closed { reason, .. } => {
                        if envelope != self.repair_envelope(&call, reason)? {
                            return Err(integrity(
                                "recovery notice differs from recorded tool state",
                            ));
                        }
                        let (parents, sources) = self.repair_provenance(&call);
                        call.state = match call.state {
                            ToolCallState::Queued => ToolCallState::NotStarted,
                            ToolCallState::Started => ToolCallState::Uncertain,
                            ToolCallState::Returned | ToolCallState::Failed => call.state,
                            _ => return Err(integrity("tool slot is already closed")),
                        };
                        (
                            EventOrigin::Harness,
                            EventKind::Intervention,
                            parents,
                            sources,
                            1,
                        )
                    }
                    _ => unreachable!(),
                };
                if value.body().origin != origin
                    || value.body().kind != kind
                    || value.body().depth != depth
                    || value.body().sources != sources
                    || value.parents() != &parents
                {
                    return Err(integrity(
                        "tool delivery origin, sources, or causal links differ",
                    ));
                }
                self.events.insert(*event, value);
                call.delivery = Some(*event);
                self.transcript.push(*event);
                self.restored_transcript.push(*event);
            }
        }
        self.tool_calls.insert(id, call);
        Ok(())
    }

    fn validate_source_attachment(
        &self,
        call: &ToolCallStatus,
        sources: &[ContentId],
    ) -> Result<()> {
        if !matches!(call.state, ToolCallState::Returned | ToolCallState::Failed)
            || call.delivery.is_some()
            || call
                .returned
                .and_then(|id| self.events.get(&id))
                .is_none_or(|event| event.body().origin != EventOrigin::Tool)
        {
            return Err(integrity(
                "source attachment requires an undelivered external return",
            ));
        }
        if sources.iter().any(|id| call.retained_sources.contains(id)) {
            return Err(integrity("duplicate retained tool source"));
        }
        self.validate_retained_sources(call, sources)
    }

    fn validate_retained_sources(
        &self,
        call: &ToolCallStatus,
        sources: &[ContentId],
    ) -> Result<()> {
        if sources.iter().copied().collect::<BTreeSet<_>>().len() != sources.len() {
            return Err(integrity("duplicate retained tool source"));
        }
        for source in sources {
            let value = self
                .events
                .get(source)
                .ok_or_else(|| integrity("retained tool source is not admitted"))?;
            if value.body().origin != EventOrigin::Tool
                || value.body().kind != EventKind::Observation
                || !value.parents().contains(&call.reply)
            {
                return Err(integrity(
                    "retained source is not an external output from this batch",
                ));
            }
        }
        Ok(())
    }

    fn return_provenance(
        &self,
        call: &ToolCallStatus,
        bytes: &[u8],
        sources: &[ContentId],
        kind: ReturnKind,
    ) -> Result<(EventOrigin, BTreeSet<ContentId>)> {
        let mut parents = BTreeSet::from([call.id]);
        if matches!(kind, ReturnKind::Observed | ReturnKind::Failed) {
            self.validate_retained_sources(call, sources)?;
            parents.extend(sources);
            if call.call.pointer("/function/name").and_then(Value::as_str) == Some("re_read") {
                return Err(integrity(
                    "re_read must preserve retrieval or host provenance",
                ));
            }
            return Ok((EventOrigin::Tool, parents));
        }
        if !sources.is_empty() {
            return Err(integrity(
                "host and retrieval returns cannot claim external tool sources",
            ));
        }
        if matches!(kind, ReturnKind::Retrieval) {
            if call.call.pointer("/function/name").and_then(Value::as_str) != Some("re_read") {
                return Err(integrity(
                    "retrieval return requires the re_read invocation",
                ));
            }
            let text = std::str::from_utf8(bytes).map_err(integrity)?;
            let events = self
                .reread_outputs
                .get(text)
                .ok_or_else(|| integrity("retrieval return has no exact admitted receipt"))?;
            parents.extend(events.iter().copied());
        }
        Ok((EventOrigin::Harness, parents))
    }

    fn delivery_provenance(
        &self,
        call: &ToolCallStatus,
        envelope: &Value,
    ) -> Result<(EventOrigin, EventKind, BTreeSet<ContentId>, u32)> {
        if !matches!(call.state, ToolCallState::Returned | ToolCallState::Failed) {
            return Err(integrity("delivery requires an observed return"));
        }
        let id = call
            .returned
            .ok_or_else(|| integrity("return event is absent"))?;
        let original = &self.events[&id];
        let bytes = self.store.source(&original.body().payload)?;
        let same = tool_contents(envelope)
            .first()
            .is_some_and(|text| text.as_bytes() == bytes);
        if same && original.body().origin == EventOrigin::Tool {
            return Ok((
                EventOrigin::Tool,
                EventKind::Observation,
                BTreeSet::new(),
                0,
            ));
        }
        if original.depth() == 0 {
            return Ok((
                EventOrigin::Harness,
                EventKind::Intervention,
                BTreeSet::from([id]),
                1,
            ));
        }
        if !same {
            return Err(integrity(
                "cannot generate another presentation from already generated material",
            ));
        }
        // Exact transport wrapping is host protocol structure. The original
        // depth-one material remains a causal parent, not a new generation input.
        Ok((
            EventOrigin::Harness,
            EventKind::Intervention,
            BTreeSet::new(),
            1,
        ))
    }

    fn validate_delivery_slot(&self, call: &ToolCallStatus, envelope: &Value) -> Result<()> {
        if call.delivery.is_some() {
            return Err(integrity("tool invocation already has a delivery"));
        }
        if self
            .tool_order
            .iter()
            .take_while(|id| **id != call.id)
            .any(|id| self.tool_calls[id].delivery.is_none())
        {
            return Err(integrity(
                "tool delivery would reorder unresolved invocation slots",
            ));
        }
        let id = call.call["id"].as_str().unwrap_or_default();
        let valid = match self.format.as_deref() {
            Some("responses") => {
                envelope["type"] == "function_call_output"
                    && envelope["call_id"] == id
                    && envelope["output"].is_string()
            }
            Some("ollama") => {
                envelope["role"] == "tool"
                    && envelope["content"].is_string()
                    && envelope.get("tool_call_id").is_none()
            }
            Some("openai" | "anthropic") => {
                envelope["role"] == "tool"
                    && envelope["tool_call_id"] == id
                    && envelope["content"].is_string()
            }
            _ => false,
        };
        let field_count = if self.format.as_deref() == Some("ollama") {
            2
        } else {
            3
        };
        if !valid
            || envelope
                .as_object()
                .is_none_or(|fields| fields.len() != field_count)
        {
            return Err(integrity(
                "tool delivery does not match its original provider slot",
            ));
        }
        Ok(())
    }

    fn repair_envelope(&self, call: &ToolCallStatus, reason: &str) -> Result<Value> {
        let fact = match call.state {
            ToolCallState::Queued => "The harness did not start this call.".into(),
            ToolCallState::Started => "The harness did not observe this call return. It may have had side effects; verify state before deciding whether to issue a new call.".into(),
            ToolCallState::Returned | ToolCallState::Failed => format!("The harness observed {} but did not finish its presentation. Retained return: {}. Additional retained sources: {}. Use re_read for bounded access.",
                if call.state == ToolCallState::Failed { "a typed tool failure" } else { "a return" },
                call.returned.ok_or_else(|| integrity("observed return is absent"))?,
                call.retained_sources.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")),
            _ => return Err(integrity("tool invocation has no unresolved protocol slot")),
        };
        let text = format!(
            "Harness notice for invocation {}: {fact} Reason: {reason}",
            call.id
        );
        let id = &call.call["id"];
        Ok(match self.format.as_deref() {
            Some("responses") => json!({"type":"function_call_output","call_id":id,"output":text}),
            Some("ollama") => json!({"role":"tool","content":text}),
            Some("openai" | "anthropic") => json!({"role":"tool","tool_call_id":id,"content":text}),
            _ => return Err(integrity("unsupported tool wire format")),
        })
    }

    fn repair_provenance(
        &self,
        call: &ToolCallStatus,
    ) -> (BTreeSet<ContentId>, BTreeSet<ContentId>) {
        let mut parents = BTreeSet::from([call.id]);
        parents.extend(call.returned);
        parents.extend(call.retained_sources.iter().copied());
        let sources = call
            .returned
            .into_iter()
            .filter(|id| self.events[id].depth() == 0)
            .collect();
        (parents, sources)
    }

    fn validate_batch_messages(&self, calls: &[Value], messages: &[Value]) -> Result<()> {
        let format = self
            .format
            .as_deref()
            .ok_or_else(|| integrity("tool batch has no recorded provider format"))?;
        let normalized = calls
            .iter()
            .map(normalized_call)
            .collect::<Result<Vec<_>>>()?;
        if calls.is_empty() || normalized != calls {
            return Err(integrity(
                "tool calls are not in the normalized admission shape",
            ));
        }
        if format != "ollama" {
            let ids = calls
                .iter()
                .map(|call| call["id"].as_str().unwrap_or_default())
                .collect::<BTreeSet<_>>();
            if ids.len() != calls.len() || ids.contains("") {
                return Err(integrity(
                    "tool correlation IDs must be nonempty and unique",
                ));
            }
        }
        let wire_calls = if format == "responses" {
            let boundary = messages
                .iter()
                .rposition(|m| m["role"] == "user" || m["type"] == "function_call_output")
                .map_or(0, |index| index + 1);
            messages[boundary..]
                .iter()
                .filter(|m| m["type"] == "function_call")
                .map(normalized_call)
                .collect::<Result<Vec<_>>>()?
        } else if matches!(format, "ollama" | "openai" | "anthropic") {
            let message = messages
                .last()
                .ok_or_else(|| integrity("batch assistant envelope is absent"))?;
            if message["role"] != "assistant" {
                return Err(integrity("batch must end at its assistant tool calls"));
            }
            message["tool_calls"]
                .as_array()
                .ok_or_else(|| integrity("batch assistant calls are absent"))?
                .iter()
                .map(normalized_call)
                .collect::<Result<Vec<_>>>()?
        } else {
            return Err(integrity("unsupported tool wire format"));
        };
        if wire_calls != calls {
            return Err(integrity(
                "batch calls differ from the retained assistant envelope",
            ));
        }
        Ok(())
    }
}

fn normalized_call(call: &Value) -> Result<Value> {
    let function = call.get("function").filter(|value| !value.is_null());
    let arguments = if function.is_none() && call.get("input").is_some() {
        &call["input"]
    } else {
        &function.unwrap_or(call)["arguments"]
    };
    let function = function.unwrap_or(call);
    let name = function["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| integrity("tool function name is absent"))?;
    let arguments = if let Some(text) = arguments.as_str() {
        serde_json::from_str(text).map_err(integrity)?
    } else {
        arguments.clone()
    };
    if !arguments.is_object() {
        return Err(integrity("tool arguments must be a normalized object"));
    }
    let id = call
        .get("call_id")
        .or_else(|| call.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(json!({"id":id,"function":{"name":name,"arguments":arguments}}))
}
