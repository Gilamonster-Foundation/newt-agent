//! Provider-plugin backend: spawn an opt-in subprocess (e.g.
//! `newt-provider-openai`) and forward `complete()` calls as JSON-RPC over
//! stdio per the schema in `plugins-protocol`.
//!
//! This is the **only** way cloud LLMs reach Newt. The default binary does
//! not link any cloud client — installing a provider plugin (`pip install
//! newt-provider-openai`) is the act of opting in.
//!
//! # Agent-key delegation (phase 1c, issue #35)
//!
//! When a Newt host spawns a provider plugin it can hand the plugin an
//! **already-attenuated** [`AgentKey`] cert chain — base64-encoded JSON,
//! delivered through the [`AGENT_KEY_ENV`](plugins_protocol::AGENT_KEY_ENV)
//! environment variable. The cert chain is signed end to end; the plugin
//! verifies it and uses the contained `Caveats` as the authority for every
//! tool dispatch it makes. Attenuation is enforced at mint time by
//! `AgentKey::delegate` (the parent's authority structurally dominates the
//! child's) — `ProviderPluginBackend` itself stores only the opaque
//! envelope string, since this workspace crate cannot depend on
//! `agent-mesh-protocol` (see `docs/decisions/mesh_integration.md`).
//!
//! Callers that hold an `AgentKey` (e.g. `newt-mesh`) build the envelope
//! via `newt_mesh::plugin_envelope::serialize_for_plugin` and pass it to
//! [`ProviderPluginBackend::with_agent_key_envelope`]. Backwards-compatible:
//! a host that doesn't yet thread an agent key leaves the field as `None`
//! and the plugin runs with whatever ambient authority it had before.

use std::process::Stdio;

use async_trait::async_trait;
use newt_core::router::Tier;
use plugins_protocol::AGENT_KEY_ENV;
use tokio::process::Command;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

pub struct ProviderPluginBackend {
    name: String,
    command: String,
    model_id: String,
    tiers: Vec<Tier>,
    /// Base64-encoded JSON [`CertChain`] for this plugin's attenuated
    /// authority. Opaque at this layer; the plugin process decodes and
    /// verifies it on startup.
    ///
    /// `None` means "no agent key threaded through" — phase-1c
    /// back-compat path. Phase 1c-aware plugins SHOULD still run, but
    /// without per-peer caveat tightening.
    ///
    /// See the module docs for how callers construct this string.
    agent_key_envelope: Option<String>,
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
            agent_key_envelope: None,
        }
    }

    /// Builder: attach an attenuated agent-key envelope that will be set
    /// in the plugin subprocess's [`AGENT_KEY_ENV`] env var on every
    /// spawn.
    ///
    /// The envelope is opaque at this layer — pass the string returned by
    /// `newt_mesh::plugin_envelope::serialize_for_plugin` (or any
    /// agent-mesh-aware helper that produces the same base64-JSON
    /// `CertChain` shape).
    ///
    /// Calling this multiple times replaces the previous envelope.
    #[must_use]
    pub fn with_agent_key_envelope(mut self, envelope: impl Into<String>) -> Self {
        let s = envelope.into();
        self.agent_key_envelope = if s.is_empty() { None } else { Some(s) };
        self
    }

    /// Borrow the currently configured envelope, if any. Test/visibility
    /// hook for callers that want to confirm threading (e.g.
    /// `newt-mesh::plugin_envelope` round-trip tests).
    #[must_use]
    pub fn agent_key_envelope(&self) -> Option<&str> {
        self.agent_key_envelope.as_deref()
    }

    /// Spawn the plugin subprocess with the agent-key envelope threaded
    /// through the [`AGENT_KEY_ENV`] env var, when present.
    ///
    /// This is the spawn primitive every JSON-RPC handshake will share
    /// once `complete()` lands its full wire path. Surfaced as `pub` so
    /// tests (and future call sites) can exercise the env-var threading
    /// without going through the still-unimplemented `complete()` path.
    ///
    /// The returned [`Command`] has stdin/stdout configured for
    /// JSON-RPC piping (`Stdio::piped()`) and stderr left as inherited
    /// so plugin logs reach the host's tracing pipeline.
    ///
    /// Phase 1d will move the envelope off the env var onto a stdin
    /// handshake — at that point this helper grows a handshake step but
    /// the wire format (base64'd JSON) is unchanged.
    #[must_use]
    pub fn spawn_command(&self) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(envelope) = &self.agent_key_envelope {
            cmd.env(AGENT_KEY_ENV, envelope);
        } else {
            // Defense: clear any inherited NEWT_AGENT_KEY so a plugin
            // can't get more authority than the host explicitly granted.
            // A parent process that *itself* has the env var set would
            // otherwise silently pass it through, defeating the whole
            // point of explicit threading.
            cmd.env_remove(AGENT_KEY_ENV);
        }
        cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_envelope_to_none() {
        let b = ProviderPluginBackend::new("openai", "newt-provider-openai", "gpt-4", vec![]);
        assert_eq!(b.agent_key_envelope(), None);
    }

    #[test]
    fn with_agent_key_envelope_stores_value() {
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![])
            .with_agent_key_envelope("abc123==");
        assert_eq!(b.agent_key_envelope(), Some("abc123=="));
    }

    #[test]
    fn empty_envelope_is_treated_as_none() {
        // An empty envelope means "the caller passed in nothing useful"
        // — store as None so spawn_command actively strips inherited
        // NEWT_AGENT_KEY rather than passing an empty value down.
        let b =
            ProviderPluginBackend::new("openai", "x", "gpt-4", vec![]).with_agent_key_envelope("");
        assert_eq!(b.agent_key_envelope(), None);
    }

    #[test]
    fn supports_tier_matches_constructor_tiers() {
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![Tier::Fast, Tier::Complex]);
        assert!(b.supports_tier(Tier::Fast));
        assert!(b.supports_tier(Tier::Complex));
        assert!(!b.supports_tier(Tier::Standard));
    }
}
