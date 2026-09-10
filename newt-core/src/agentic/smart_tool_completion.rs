//! Durable invocation lifetime, shared by every primary provider's dispatcher.
use super::{SmartHarness, Value};
use agent_harness::{ToolCallState, ToolReturn};
use content_addressable::ContentId;
use std::sync::Mutex;

pub(crate) struct ToolBatch<'a> {
    harness: &'a SmartHarness,
    ids: Vec<ContentId>,
}

impl<'a> ToolBatch<'a> {
    pub(super) fn new(harness: &'a SmartHarness, ids: Vec<ContentId>) -> Self {
        Self { harness, ids }
    }

    fn id(&self, index: usize) -> anyhow::Result<ContentId> {
        self.ids
            .get(index)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("tool invocation ordinal is absent"))
    }

    pub(crate) fn start<'b>(
        &'b self,
        index: usize,
        disclosure: Option<&'b crate::ocap::DisclosureFilter>,
    ) -> anyhow::Result<ToolInvocation<'b>> {
        let id = self.id(index)?;
        let mut state = self.harness.state()?;
        let name = state.session.tool_call(id)?.call["function"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invocation has no tool name"))?
            .to_owned();
        state.session.start_tool_call(id)?;
        Ok(ToolInvocation {
            harness: self.harness,
            id,
            name,
            disclosure,
            output: Mutex::new(InvocationOutput::default()),
        })
    }

    fn resolve(&self, index: usize, message: &Value) -> anyhow::Result<()> {
        Ok(self
            .harness
            .state()?
            .session
            .resolve_tool_call(self.id(index)?, message)?)
    }
}

impl Drop for ToolBatch<'_> {
    fn drop(&mut self) {
        let _ = self
            .harness
            .interrupt_tools("tool batch ended before all deliveries");
    }
}

#[derive(Clone, Copy, Default)]
enum ReturnOrigin {
    #[default]
    Observed,
    Host,
    Retrieval,
}

#[derive(Default)]
struct InvocationOutput {
    origin: ReturnOrigin,
    returned: Option<ContentId>,
    text: String,
    sources: Vec<(String, ContentId)>,
}

/// A smart dispatcher cannot hold a harness without its started invocation.
pub(crate) struct ToolInvocation<'a> {
    harness: &'a SmartHarness,
    id: ContentId,
    name: String,
    disclosure: Option<&'a crate::ocap::DisclosureFilter>,
    output: Mutex<InvocationOutput>,
}

impl ToolInvocation<'_> {
    pub(crate) fn harness(&self) -> &SmartHarness {
        self.harness
    }

    pub(crate) fn host(&self) {
        self.output
            .lock()
            .expect("invocation provenance lock poisoned")
            .origin = ReturnOrigin::Host;
    }

    pub(crate) fn retrieval(&self) {
        self.output
            .lock()
            .expect("invocation provenance lock poisoned")
            .origin = ReturnOrigin::Retrieval;
    }

    /// Called before display or provider bookkeeping. Returned text is an
    /// observation, including empty/error strings; it is never a success claim.
    pub(crate) fn observe(
        &self,
        result: &str,
        spill: Option<&dyn crate::agentic::content_spill::SpillStore>,
    ) -> anyhow::Result<()> {
        self.observe_return(result, spill).map_err(|error| {
            // The model-visible repair names the failure, while the observed
            // bytes stay behind their retained CID (including invalid markers).
            let interruption_reason = format!("{error:#}");
            let disclosed = crate::agentic::redact_model_facing(self.disclosure, result.to_owned());
            let disclosed = crate::agentic::compress::redact_secrets(&disclosed);
            let error = error.context(format!(
                "tool completion failed for {}; observed return: {disclosed}",
                self.name
            ));
            match self.harness.interrupt_tools(&interruption_reason) {
                Ok(()) => error,
                Err(interruption) => error.context(format!(
                    "could not persist tool interruption: {interruption:#}"
                )),
            }
        })
    }

    fn observe_return(
        &self,
        result: &str,
        spill: Option<&dyn crate::agentic::content_spill::SpillStore>,
    ) -> anyhow::Result<()> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("invocation output lock poisoned"))?;
        anyhow::ensure!(output.returned.is_none(), "tool return already observed");
        let mut text = crate::agentic::redact_model_facing(self.disclosure, result.to_owned());
        if !matches!(output.origin, ReturnOrigin::Retrieval) {
            text = crate::agentic::compress::redact_secrets(&text);
        }
        // The host has received these bytes. Publish that fact before parsing
        // textual handles or touching any optional retained source.
        let returned = match output.origin {
            ReturnOrigin::Observed => ToolReturn::Observed {
                bytes: text.as_bytes(),
                retained_sources: &[],
            },
            ReturnOrigin::Host => ToolReturn::Host(&text),
            ReturnOrigin::Retrieval => ToolReturn::Retrieval(&text),
        };
        output.returned = Some(
            self.harness
                .state()?
                .session
                .record_tool_return(self.id, returned)?,
        );
        output.text = text;
        if matches!(output.origin, ReturnOrigin::Observed) {
            let mut markers = Vec::new();
            crate::agentic::responses_wire_validation::extract_markers(&output.text, &mut markers);
            for (kind, handle) in markers {
                if kind != crate::agentic::responses_wire_validation::MarkerKind::Spill {
                    continue;
                }
                let hint = crate::agentic::content_spill::tool_output_retrieval_hint(&handle);
                if !output.text.contains(&hint)
                    || output.sources.iter().any(|(seen, _)| seen == &hint)
                {
                    continue;
                }
                let cid = crate::agentic::content_spill::SpillCid::parse(&handle)?;
                let record = spill.and_then(|store| store.fetch(&cid)).ok_or_else(|| {
                    anyhow::anyhow!(
                        "retained tool output is absent from its authorized spill store"
                    )
                })?;
                anyhow::ensure!(
                    matches!(
                        record.provenance,
                        crate::agentic::content_spill::SpillProvenance::ToolOutput { .. }
                    ),
                    "retained spill source is not an external tool output"
                );
                let full =
                    crate::agentic::redact_model_facing(self.disclosure, record.redacted_text);
                let mut state = self.harness.state()?;
                let cid = state
                    .session
                    .retain_tool_output(&self.name, full.as_bytes())?;
                state.session.record_tool_sources(self.id, &[cid])?;
                output.sources.push((hint, cid));
            }
        }
        Ok(())
    }

    pub(crate) fn model_text(&self) -> anyhow::Result<String> {
        let output = self
            .output
            .lock()
            .map_err(|_| anyhow::anyhow!("invocation output lock poisoned"))?;
        let returned = output
            .returned
            .ok_or_else(|| anyhow::anyhow!("tool output was not durably observed"))?;
        if matches!(output.origin, ReturnOrigin::Retrieval | ReturnOrigin::Host) {
            return Ok(output.text.clone());
        }
        let mut display = output.text.clone();
        let mut slices = Vec::new();
        for (hint, cid) in &output.sources {
            let source = self.harness.tool_slice(*cid)?;
            let replacement = format!("Call re_read with JSON arguments {} for bounded retained output; use next_offset to continue.", serde_json::json!({"cid":cid}));
            display = display.replace(hint, &replacement);
            slices.push(source);
        }
        if slices.is_empty() {
            if display.len() <= self.harness.settings.initial_tool_bytes {
                return Ok(display);
            }
            slices.push(self.harness.tool_slice(returned)?);
            display = format!("{} returned {} bytes. The initial slice follows; use re_read with source_cid and next_offset for more.", self.name, display.len());
        }
        Ok(serde_json::to_string(
            &serde_json::json!({"display":display,"sources":slices}),
        )?)
    }

    fn deliver(&self, message: &Value) -> anyhow::Result<()> {
        Ok(self
            .harness
            .state()?
            .session
            .record_tool_delivery(self.id, message)?)
    }
}

impl Drop for ToolInvocation<'_> {
    fn drop(&mut self) {
        let pending = self.harness.state().and_then(|state| {
            Ok(state.session.tool_call(self.id)?.state == ToolCallState::Started)
        });
        if !matches!(pending, Ok(false)) {
            let _ = self
                .harness
                .interrupt_tools("tool future dropped without an observed return");
        }
    }
}

impl SmartHarness {
    fn interrupt_tools(&self, reason: &str) -> anyhow::Result<()> {
        // Drop cannot propagate, so failures latch for every later accessor.
        // Ordinary error paths also include this error in their returned chain.
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("smart harness state lock poisoned"))?;
        if let Some(error) = &state.deferred_failure {
            anyhow::bail!("{error}");
        }
        if let Err(error) = state.session.interrupt_tool_batch(reason) {
            let error = format!("tool interruption persistence failed: {error}");
            state.deferred_failure = Some(error.clone());
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

pub(crate) fn push_tool_return(
    messages: &mut Vec<Value>,
    message: Value,
    invocation: Option<&ToolInvocation<'_>>,
) -> anyhow::Result<()> {
    if let Some(invocation) = invocation {
        invocation.deliver(&message)?;
    }
    messages.push(message);
    Ok(())
}

pub(crate) fn push_tool_resolution(
    messages: &mut Vec<Value>,
    message: Value,
    batch: Option<&ToolBatch<'_>>,
    index: usize,
) -> anyhow::Result<()> {
    if let Some(batch) = batch {
        batch.resolve(index, &message)?;
    }
    messages.push(message);
    Ok(())
}

#[cfg(test)]
impl SmartHarness {
    /// Admits a real normalized call envelope for dispatcher fixtures.
    pub(crate) fn fixture_tool_batch(&self, name: &str, args: Value) -> ToolBatch<'_> {
        let mut messages = self.state().unwrap().session.restored_messages().unwrap();
        messages.push(serde_json::json!({"role":"user","content":"execute fixture call"}));
        self.request(&serde_json::json!({"messages":messages}), "openai")
            .unwrap();
        let call = serde_json::json!({"id":"fixture_call","type":"function","function":{"name":name,"arguments":args}});
        let assistant = serde_json::json!({"role":"assistant","content":"","tool_calls":[call]});
        self.observe(
            &serde_json::to_vec(&serde_json::json!({"choices":[{"message":assistant}]})).unwrap(),
        )
        .unwrap();
        messages.push(assistant);
        self.tool_batch(
            &[crate::agentic::tools::ValidatedCall {
                call_id: "fixture_call".into(),
                name: name.into(),
                args,
            }],
            &messages,
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn long_host_return_is_delivered_without_deriving_another_generation() {
        let harness = SmartHarness::new(
            agent_harness::Session::new(Default::default()).unwrap(),
            Arc::new(|_| panic!("no inference")),
            super::super::AdjudicationSettings {
                initial_tool_bytes: 8,
                ..Default::default()
            },
        )
        .unwrap();
        let batch = harness.fixture_tool_batch("re_read", serde_json::json!({"cid":"absent"}));
        let invocation = batch.start(0, None).unwrap();
        let text = "Error: re_read refused: the requested CID is outside this session";
        invocation.host();
        invocation.observe(text, None).unwrap();
        let model_text = invocation.model_text().unwrap();
        assert_eq!(model_text, text);
        invocation.deliver(&serde_json::json!({"role":"tool","tool_call_id":"fixture_call","content":model_text})).unwrap();
    }
}

#[cfg(test)]
mod spill_failures {
    use super::*;
    use crate::agentic::content_spill::{SessionSpillStore, SpillProvenance, SpillStore};
    use std::sync::Arc;

    fn returned_hint_survives_failure(kind: &str) {
        let directory = tempfile::tempdir().unwrap();
        let harness = SmartHarness::new(
            agent_harness::Session::open(directory.path(), Default::default()).unwrap(),
            Arc::new(|_| panic!("no inference")),
            Default::default(),
        )
        .unwrap();
        let batch = harness.fixture_tool_batch("read_file", serde_json::json!({"path":"fixture"}));
        let invocation = batch.start(0, None).unwrap();
        let id = invocation.id;
        let store = SessionSpillStore::new([3; 16]);
        let provenance = if kind == "compaction" {
            SpillProvenance::CompactionSpan
        } else {
            SpillProvenance::ToolOutput {
                tool_name: Some("read_file".into()),
            }
        };
        let foreign = SessionSpillStore::new([9; 16]);
        let owner = if kind == "foreign" { &foreign } else { &store };
        let staged = owner
            .stage(provenance, "stored source payload".into())
            .unwrap();
        if matches!(kind, "compaction" | "foreign") {
            owner.commit_batch(std::slice::from_ref(&staged)).unwrap();
        }
        let handle = if kind == "malformed" {
            "b".repeat(59)
        } else {
            staged.handle()
        };
        let raw = format!(
            "literal file content with a suspicious hint:\n{}",
            crate::agentic::content_spill::tool_output_retrieval_hint(&handle)
        );
        let error = invocation
            .observe(&raw, Some(&store))
            .expect_err("invalid source retention stops delivery");
        assert!(format!("{error:#}").contains(&raw));
        assert_eq!(
            harness
                .state()
                .unwrap()
                .session
                .tool_call(id)
                .unwrap()
                .state,
            ToolCallState::Returned,
            "the raw return must precede hint processing"
        );
        drop(invocation);
        drop(batch);
        let head = harness.head().unwrap();
        drop(harness);
        drop(store);
        let mut restored =
            agent_harness::Session::restore(directory.path(), head, "local-session").unwrap();
        let call = restored.tool_call(id).unwrap();
        assert_eq!(call.state, ToolCallState::Returned);
        assert!(
            call.retained_sources.is_empty(),
            "generated/absent sources cannot be relabeled as external tool evidence"
        );
        let returned = call.returned.unwrap();
        assert_eq!(
            restored.re_read(&returned.to_string(), 0, 4096).unwrap()["text"],
            raw
        );
    }

    /// Grounds missing authorized spill membership in a cold disk restore of
    /// the already-returned tool String, rather than inventing uncertainty.
    #[test]
    fn missing_spill_keeps_the_actual_return() {
        returned_hint_survives_failure("missing");
    }

    /// Grounds malformed textual handles in the same durable-return boundary.
    #[test]
    fn malformed_spill_keeps_the_actual_return() {
        returned_hint_survives_failure("malformed");
    }

    /// Grounds source provenance with a real committed compaction record: its
    /// generated bytes cannot become external Tool evidence through a hint.
    #[test]
    fn generated_spill_is_refused_without_losing_the_actual_return() {
        returned_hint_survives_failure("compaction");
    }
    /// Grounds real per-session spill membership: another store's committed
    /// tool output cannot authorize retention in the active session.
    #[test]
    fn foreign_spill_keeps_the_actual_return() {
        returned_hint_survives_failure("foreign");
    }
}
