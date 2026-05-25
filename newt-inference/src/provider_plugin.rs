//! Provider-plugin backend: spawn an opt-in subprocess (e.g.
//! `newt-provider-openai`) and forward `complete()` calls as JSON-RPC over
//! stdio per the schema in `plugins-protocol`.
//!
//! This is the **only** way cloud LLMs reach Newt. The default binary does
//! not link any cloud client — installing a provider plugin (`pip install
//! newt-provider-openai`) is the act of opting in.

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

pub struct ProviderPluginBackend {
    name: String,
    command: String,
    model_id: String,
    tiers: Vec<Tier>,
}

impl ProviderPluginBackend {
    pub fn new(
        name: impl Into<String>,
        command: impl Into<String>,
        model_id: impl Into<String>,
        tiers: Vec<Tier>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            model_id: model_id.into(),
            tiers,
        }
    }
}

#[async_trait]
impl InferenceBackend for ProviderPluginBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supports_tier(&self, tier: Tier) -> bool {
        self.tiers.contains(&tier)
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        anyhow::bail!(
            "ProviderPluginBackend.complete not yet implemented (command={})",
            self.command
        )
    }
}
