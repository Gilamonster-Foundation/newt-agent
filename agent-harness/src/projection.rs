//! Ordered prompt rendering. A Merkle parent set cannot carry presentation order.
use agent_frame::Span;
use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub event: ContentId,
    pub source: RawContentId,
    pub role: String,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub schema: u32,
    pub renderer: String,
    pub field: String,
    pub template: RawContentId,
    pub entries: Vec<Entry>,
}

impl Projection {
    /// Replay the host's serialized request. Missing or redacted inputs fail;
    /// this says nothing about transformations inside a remote provider.
    pub fn render(&self, store: &crate::store::FrameStore) -> crate::Result<Vec<u8>> {
        if self.schema != 1
            || !matches!(
                self.renderer.as_str(),
                "json-messages-v1" | "anthropic-messages-v1"
            )
            || !matches!(self.field.as_str(), "messages" | "input")
        {
            return Err(crate::Error::Integrity(
                "unsupported projection renderer or schema".into(),
            ));
        }
        let mut body: serde_json::Value =
            serde_json::from_slice(&store.source(&self.template)?).map_err(invalid)?;
        let object = body
            .as_object_mut()
            .ok_or_else(|| invalid("request template is not an object"))?;
        if object.get(&self.field) != Some(&serde_json::Value::Array(Vec::new())) {
            return Err(invalid(
                "request template must contain an empty message array",
            ));
        }
        let messages = self
            .entries
            .iter()
            .map(|entry| {
                let source = store.source(&entry.source)?;
                let slice = entry
                    .span
                    .slice(&source)
                    .ok_or_else(|| invalid("selected span is absent"))?;
                let message: serde_json::Value = serde_json::from_slice(slice).map_err(invalid)?;
                if role(&message) != entry.role {
                    return Err(invalid("recorded role differs from selected material"));
                }
                Ok(message)
            })
            .collect::<crate::Result<Vec<_>>>()?;
        let messages = if self.renderer == "anthropic-messages-v1" {
            if self.field != "messages" {
                return Err(invalid("Anthropic rendering requires messages"));
            }
            if messages.iter().any(|message| {
                message
                    .get("content")
                    .is_some_and(|content| !content.is_null() && !content.is_string())
            }) {
                return Err(invalid("Anthropic source rendering requires canonical text messages; use the native request path for structured content"));
            }
            let (system, messages) = crate::render::anthropic_wire_messages(&messages)?;
            match system {
                Some(system) => {
                    object.insert("system".into(), serde_json::Value::String(system));
                }
                None => {
                    object.remove("system");
                }
            }
            messages
        } else {
            messages
        };
        object.insert(self.field.clone(), serde_json::Value::Array(messages));
        serde_json::to_vec(&body).map_err(invalid)
    }
}

/// Responses function items do not carry a role; their item type determines it.
pub fn role(message: &serde_json::Value) -> &str {
    message
        .get("role")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(
            || match message.get("type").and_then(serde_json::Value::as_str) {
                Some("function_call_output") => "tool",
                Some("function_call" | "reasoning") => "assistant",
                _ => "user",
            },
        )
}

fn invalid(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::Integrity(error.to_string())
}

impl ContentAddressable for Projection {
    fn canonical_form(&self) -> std::result::Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
