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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[async_trait]
pub trait InferenceBackend: Send + Sync {
    fn name(&self) -> &str;
    fn model_id(&self) -> &str;
    fn supports_tier(&self, tier: Tier) -> bool;
    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply>;
}
