//! Newt-Agent provider-plugin protocol.
//!
//! Provider plugins run as separate processes and speak JSON-RPC over stdio.
//! They register opt-in inference backends — most notably the cloud
//! backends (OpenAI, Anthropic) that the default Newt binary deliberately
//! does not link.
//!
//! v0 surface: `initialize`, `list_models`, `complete`, `stream`, `shutdown`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeRequest {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    pub plugin_name: String,
    pub plugin_version: String,
    pub supported_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResponse {
    pub content: String,
    pub model_id: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub const PROTOCOL_VERSION: u32 = 0;
