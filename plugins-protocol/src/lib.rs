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

/// Emission shapes a coder plugin can produce, surfaced in
/// `TaskReply.emission_shape` when the newt-coder plugin processed the
/// request.
///
/// Downstream consumers (drake-foreman scorecard, audit logs, the
/// pilot dashboard) compare against these constants so the wire-level
/// strings can't drift between producer and consumer.
///
/// The taxonomy is documented in
/// `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`.
pub mod emission_shape {
    /// One or more `FILE: <path>\n<contents>\nEND-FILE` blocks — the
    /// S5 whole-file-emit strategy's preferred shape.
    pub const WHOLE_FILES: &str = "whole_files";

    /// A unified diff (fenced or unfenced). Legacy path; useful when a
    /// model ignores the whole-file directive but lands a valid hunk.
    pub const UNIFIED_DIFF: &str = "unified_diff";

    /// No structured emission detected; the model emitted prose only
    /// (failure mode T0a in the taxonomy).
    pub const PROSE: &str = "prose";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emission_shape_constants_are_stable_strings() {
        // These constants are part of the wire protocol. Changing them
        // breaks every downstream consumer; pin them with an explicit
        // test so a careless rename fails CI loudly.
        assert_eq!(emission_shape::WHOLE_FILES, "whole_files");
        assert_eq!(emission_shape::UNIFIED_DIFF, "unified_diff");
        assert_eq!(emission_shape::PROSE, "prose");
    }
}
