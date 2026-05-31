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

/// Environment variable a Newt host sets when spawning a provider plugin to
/// hand the plugin a base64-encoded JSON [`CertChain`] (an attenuated
/// [`AgentKey`]) for this dispatch.
///
/// Phase 1c transport (issue #35). The host minted this child cert by
/// calling [`AgentKey::delegate`] on its own parent key — so the chain is
/// signed end-to-end, attenuation is structurally enforced, and the plugin
/// can verify the chain locally without a separate trust anchor (the chain
/// roots at the user's `UserPublic`, which is embedded in the leaf cert).
///
/// Plugins that **don't** read this env var run with whatever ambient
/// authority they had before phase 1c — that's a deliberate
/// back-compat behavior for older plugins. Plugins built against phase 1c
/// or later **SHOULD** read this and use it as the source of truth for
/// every tool dispatch they make.
///
/// Why env var (and not a `CompleteRequest` field)?
/// - Simpler — no protocol-version bump required, older plugins ignore it.
/// - Per-process — the cert is attached to the plugin's lifetime, not to
///   any individual call.
/// - Phase 1d can swap env-var transport for a stdin handshake without
///   changing the wire JSON shape (just stop reading env, start reading
///   stdin). The wire format (base64'd CertChain JSON) is stable.
///
/// **Caveat:** env vars on Unix are visible to other processes running as
/// the same uid (via `/proc/$PID/environ`). For 35c this is acceptable
/// because the plugin and host run with the same authority anyway — the
/// adversary model is a confused plugin, not a same-uid attacker reading
/// `/proc`. Phase 1d hardens this by moving the handshake to stdin.
///
/// [`AgentKey`]: https://docs.rs/agent-mesh-protocol/latest/agent_mesh_protocol/agent_key/struct.AgentKey.html
/// [`AgentKey::delegate`]: https://docs.rs/agent-mesh-protocol/latest/agent_mesh_protocol/agent_key/struct.AgentKey.html#method.delegate
/// [`CertChain`]: https://docs.rs/agent-mesh-protocol/latest/agent_mesh_protocol/agent_key/struct.CertChain.html
pub const AGENT_KEY_ENV: &str = "NEWT_AGENT_KEY";

/// Read the agent-key envelope from the [`AGENT_KEY_ENV`] env var, if set.
///
/// Plugin-side helper. Returns `None` if the variable is not set or is empty
/// — back-compat with hosts and plugins built before phase 1c.
///
/// This helper deliberately does **no** decoding or verification: the value
/// is an opaque base64 string here. Plugins that link `newt-mesh` (or
/// roll their own agent-mesh import) consume that string with
/// `newt_mesh::plugin_envelope::caveats_from_envelope`, which decodes,
/// signature-checks the chain, and extracts the attenuated [`Caveats`].
///
/// Keeping verification out of `plugins-protocol` is intentional: this
/// crate is the *workspace* crate, and the workspace forbids depending on
/// `agent-mesh-protocol` (see the workspace `exclude` list and
/// `docs/decisions/mesh_integration.md`). Plugins that don't need
/// cryptographic verification can still call this helper and treat the
/// returned string as opaque.
#[must_use]
pub fn read_agent_key_envelope_from_env() -> Option<String> {
    match std::env::var(AGENT_KEY_ENV) {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

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

    #[test]
    fn agent_key_env_name_is_stable() {
        // Wire-protocol contract: host and plugin agree on this name.
        // A rename without coordinated update breaks every phase-1c
        // plugin in the wild.
        assert_eq!(AGENT_KEY_ENV, "NEWT_AGENT_KEY");
    }

    // The env-reader helper is racy when tested in parallel (other
    // tests in this binary may set/unset the same variable). Guard the
    // two cases with a mutex so they don't trample each other.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn read_agent_key_envelope_returns_none_when_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: std::env::remove_var is unsafe in 2024 edition.
        // We're in 2021 edition where it's safe; the lock above
        // serializes access to the env var across this test binary.
        std::env::remove_var(AGENT_KEY_ENV);
        assert_eq!(read_agent_key_envelope_from_env(), None);
    }

    #[test]
    fn read_agent_key_envelope_returns_value_when_set() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(AGENT_KEY_ENV, "abc123==");
        assert_eq!(
            read_agent_key_envelope_from_env(),
            Some("abc123==".to_string())
        );
        std::env::remove_var(AGENT_KEY_ENV);
    }

    #[test]
    fn read_agent_key_envelope_treats_empty_as_none() {
        // An empty env var is semantically "not set" — no provider
        // plugin will get useful work out of a zero-byte envelope, and
        // treating it as `Some("")` would force every consumer to
        // re-check for emptiness.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(AGENT_KEY_ENV, "");
        assert_eq!(read_agent_key_envelope_from_env(), None);
        std::env::remove_var(AGENT_KEY_ENV);
    }
}
