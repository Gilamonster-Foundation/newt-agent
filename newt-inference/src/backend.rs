use async_trait::async_trait;
use newt_core::router::Tier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatReply {
    pub content: String,
    pub model_id: String,
    /// Token usage reported by the backend, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<newt_core::TokenUsage>,
}

impl ChatReply {
    /// Format an audit-trail line: "backend=<name> model_id=<id>".
    pub fn audit_string(&self, backend_name: &str) -> String {
        format!("backend={} model_id={}", backend_name, self.model_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

impl ChatRequest {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            max_tokens: None,
        }
    }
    pub fn system(mut self, content: impl Into<String>) -> Self {
        self.messages.push(Message::system(content));
        self
    }
    pub fn user(mut self, content: impl Into<String>) -> Self {
        self.messages.push(Message::user(content));
        self
    }
    pub fn assistant(mut self, content: impl Into<String>) -> Self {
        self.messages.push(Message::assistant(content));
        self
    }
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
pub trait InferenceBackend: Send + Sync {
    fn name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn supports_tier(&self, tier: Tier) -> bool;
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply>;

    /// Optional HTTP endpoint URL the backend will hit when `complete`
    /// is called. Used by the dispatch-time `Caveats::net` enforcement
    /// in `newt-coder` to decide whether a peer's signed authority
    /// permits this network call.
    ///
    /// Returns `None` for backends that don't make a network call
    /// (mocks, subprocess plugins that bridge in-process inference,
    /// future in-tree inference). When `None`, the network-axis check
    /// is treated as vacuously satisfied — there is no host to consult.
    ///
    /// The default returns `None` so existing implementations stay
    /// source-compatible; backends that do speak HTTP (e.g.
    /// `LocalOllamaBackend`, `LocalVllmBackend`) override this.
    fn endpoint(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_user_role() {
        let msg = Message::user("hi");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "hi");
    }

    #[test]
    fn message_system_role() {
        let msg = Message::system("x");
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, "x");
    }

    #[test]
    fn message_assistant_role() {
        let msg = Message::assistant("x");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "x");
    }

    #[test]
    fn chat_request_builder_chain() {
        let req = ChatRequest::new()
            .system("You are helpful.")
            .user("Hello")
            .max_tokens(256);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.max_tokens, Some(256));
    }

    #[test]
    fn chat_request_serde_roundtrip() {
        let req = ChatRequest::new()
            .system("sys")
            .user("hello")
            .max_tokens(100);
        let json = serde_json::to_string(&req).unwrap();
        let back: ChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[0], Message::system("sys"));
        assert_eq!(back.messages[1], Message::user("hello"));
        assert_eq!(back.max_tokens, Some(100));
    }

    #[test]
    fn chat_request_default_empty() {
        let req = ChatRequest::default();
        assert!(req.messages.is_empty());
        assert_eq!(req.max_tokens, None);
    }

    #[test]
    fn audit_string_format() {
        let reply = ChatReply {
            content: "hello".to_string(),
            model_id: "llama3.1:8b".to_string(),
            usage: None,
        };
        assert_eq!(
            reply.audit_string("ollama-local"),
            "backend=ollama-local model_id=llama3.1:8b"
        );
    }
}
