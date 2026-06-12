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
//!
//! # Issue #93 — parent-key threading (chain-rooting at the operator)
//!
//! `with_agent_key_envelope` accepts an *opaque* string and so cannot prove
//! the envelope chains back to the operator's `UserKey` from
//! `~/.newt/identity.pem`. The parent-key threading API
//! ([`ProviderPluginBackend::with_parent_key`]) eliminates that gap: the
//! backend holds an `Arc<AgentKey>` minted from the operator key (the
//! worker's session root or its attenuated dispatch key), and
//! [`ProviderPluginBackend::spawn_command`] mints a **fresh** delegated
//! child via `parent.delegate(child_metadata)` on each spawn and
//! serializes that into the envelope. The synthetic
//! `AgentKey::generate()` path that earlier prototypes used at spawn time
//! is structurally impossible here: the backend cannot mint without a
//! parent key, and the parent must come from the operator's identity.
//!
//! Precedence in `spawn_command()`:
//!
//! 1. `parent_key` set → mint+serialize a fresh child every spawn
//!    (the #93 path; root chains back to the operator).
//! 2. Else `agent_key_envelope` set → emit the stored opaque string
//!    (the pre-#93 compat path; used by tests that hand-craft the
//!    envelope).
//! 3. Else strip any inherited `NEWT_AGENT_KEY` so a confused parent
//!    process cannot leak its ambient authority.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use newt_core::router::Tier;
use newt_identity::{AgentKey, AgentMetadata};
use plugins_protocol::{InitializeRequest, PluginClient, AGENT_KEY_ENV, PROTOCOL_VERSION};
use tokio::process::Command;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

pub struct ProviderPluginBackend {
    name: String,
    command: String,
    model_id: String,
    env_pass: Vec<String>,
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
    /// Operator-rooted parent [`AgentKey`] (issue #93). When set, takes
    /// precedence over [`Self::agent_key_envelope`]:
    /// [`Self::spawn_command`] derives a fresh delegated child from this
    /// parent on every spawn and uses the resulting cert chain as the
    /// envelope. The chain therefore roots back to the operator's
    /// `UserKey` (loaded from `~/.newt/identity.pem`), never a synthetic
    /// key.
    parent_key: Option<Arc<AgentKey>>,
    /// Metadata template used when minting a delegated child from
    /// [`Self::parent_key`]. Cloned per spawn — wall-clock `issued_at`
    /// inside the metadata is a *claim* in a signed cert (not a
    /// coordination primitive), so a stable template is fine here.
    child_metadata: Option<AgentMetadata>,
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
            env_pass: Vec::new(),
            tiers,
            agent_key_envelope: None,
            parent_key: None,
            child_metadata: None,
        }
    }

    pub fn from_config(cfg: &newt_core::config::ProviderConfig) -> anyhow::Result<Self> {
        let model = cfg
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("provider '{}' is missing required model", cfg.name))?;
        Ok(Self::new(
            cfg.name.clone(),
            cfg.command.clone(),
            model.to_string(),
            cfg.tiers.clone(),
        )
        .with_env_pass(cfg.env_pass.clone()))
    }

    #[must_use]
    pub fn with_env_pass(mut self, env_pass: Vec<String>) -> Self {
        self.env_pass = env_pass;
        self
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
    ///
    /// **Issue #93:** prefer [`Self::with_parent_key`] over this method.
    /// `with_agent_key_envelope` accepts an *opaque* string and cannot
    /// prove the chain roots back to the operator's `UserKey`;
    /// `with_parent_key` takes the operator-rooted `Arc<AgentKey>`
    /// directly so the chain-rooting property holds by construction.
    /// This method remains for back-compat with the existing
    /// `newt-mesh::plugin_envelope::serialize_for_plugin` callers in the
    /// excluded-workspace path and the `tests/plugin_envelope_e2e.rs`
    /// e2e harness.
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

    /// Builder: attach an operator-rooted parent [`AgentKey`] and the
    /// metadata used to mint a delegated child for each plugin spawn
    /// (issue #93).
    ///
    /// Every call to [`Self::spawn_command`] will derive a **fresh**
    /// child via `parent.delegate(child_metadata.clone())`, serialize
    /// the resulting cert chain, and set it as `NEWT_AGENT_KEY` in the
    /// subprocess environment. The chain therefore walks back through
    /// `parent` to its root `UserKey` — the operator's
    /// `~/.newt/identity.pem` — never a synthetic key minted at spawn
    /// time.
    ///
    /// `child_metadata.caveats` MUST be `⊑ parent.cert().metadata.caveats`;
    /// `AgentKey::delegate` refuses to mint an amplifying child and the
    /// spawn fails loudly rather than silently lifting authority.
    ///
    /// When both `parent_key` and the legacy `agent_key_envelope` are
    /// set, the parent-key path wins — it's the discipline #93 enforces.
    #[must_use]
    pub fn with_parent_key(mut self, parent: Arc<AgentKey>, child_metadata: AgentMetadata) -> Self {
        self.parent_key = Some(parent);
        self.child_metadata = Some(child_metadata);
        self
    }

    /// `true` when this backend will mint per-spawn delegated children
    /// from an operator-rooted parent `AgentKey` (issue #93 path).
    /// Visibility hook for tests and the chain-rooting verification.
    #[must_use]
    pub fn has_parent_key(&self) -> bool {
        self.parent_key.is_some()
    }

    /// Borrow the configured parent key, if any. Tests use this to
    /// assert the chain-rooting property end to end.
    #[must_use]
    pub fn parent_key(&self) -> Option<&Arc<AgentKey>> {
        self.parent_key.as_ref()
    }

    /// Mint the per-spawn envelope from the configured `parent_key` and
    /// `child_metadata`. Surfaced as `pub` so a future `complete()`
    /// implementation (and the regression tests today) can inspect the
    /// derived envelope without re-running [`Self::spawn_command`].
    ///
    /// Returns `None` when no parent key is configured — the caller
    /// should fall back to [`Self::agent_key_envelope`] (the pre-#93
    /// compat path).
    pub fn mint_plugin_envelope(&self) -> Option<Result<String, newt_identity::EnvelopeError>> {
        let parent = self.parent_key.as_ref()?;
        let metadata = self.child_metadata.as_ref()?;
        Some(newt_identity::serialize_for_plugin(
            parent.as_ref(),
            metadata.clone(),
        ))
    }

    /// Spawn the plugin subprocess with the agent-key envelope threaded
    /// through the [`AGENT_KEY_ENV`] env var, when present.
    ///
    /// This is the spawn primitive every JSON-RPC handshake will share
    /// once `complete()` lands its full wire path. Surfaced as `pub` so
    /// tests (and future call sites) can exercise the env-var threading
    /// without going through the still-unimplemented `complete()` path.
    ///
    /// **Issue #93 chokepoint.** This is the *only* place a
    /// `NEWT_AGENT_KEY` value is constructed for a subprocess plugin
    /// across the headless dispatch path. It refuses to invent one: the
    /// envelope is either minted from an operator-rooted parent key
    /// (the #93 happy path), forwarded from a hand-crafted envelope (the
    /// compat path for the excluded `newt-mesh` workspace), or stripped
    /// to prevent ambient leakage. There is no `AgentKey::generate()`
    /// call inside this method's reachability cone.
    ///
    /// On mint failure (e.g. caller asked for an amplifying child),
    /// the [`NEWT_AGENT_KEY`](plugins_protocol::AGENT_KEY_ENV) env var
    /// is **stripped** rather than silently downgraded to ambient
    /// authority — the failure is logged and the plugin runs with no
    /// envelope, the same safe-fallback `with_agent_key_envelope("")`
    /// produces.
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
        cmd.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        for name in &self.env_pass {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }

        #[cfg(windows)]
        for name in ["ComSpec", "SystemRoot", "PATHEXT"] {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }

        // Precedence: parent_key (issue #93) → agent_key_envelope → strip.
        if let Some(minted) = self.mint_plugin_envelope() {
            match minted {
                Ok(envelope) => {
                    cmd.env(AGENT_KEY_ENV, envelope);
                }
                Err(e) => {
                    // Amplification or serialize failure: log + strip.
                    // We refuse to fall back to `agent_key_envelope`
                    // here — a parent key was configured precisely so
                    // its constraints would govern, and silently
                    // reverting to an unverified stored envelope would
                    // defeat that.
                    tracing::error!(
                        error = %e,
                        plugin = %self.name,
                        "ProviderPluginBackend: failed to mint plugin envelope from parent key; \
                         stripping NEWT_AGENT_KEY"
                    );
                    cmd.env_remove(AGENT_KEY_ENV);
                }
            }
        } else if let Some(envelope) = &self.agent_key_envelope {
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

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        let mut client = PluginClient::spawn_command(self.spawn_command())?;
        client
            .initialize(InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_name: "newt-agent".to_string(),
                client_version: env!("CARGO_PKG_VERSION").to_string(),
            })
            .await?;

        let response = client
            .complete(plugins_protocol::CompleteRequest {
                model: self.model_id.clone(),
                messages: req
                    .messages
                    .into_iter()
                    .map(|m| plugins_protocol::Message {
                        role: m.role,
                        content: m.content,
                    })
                    .collect(),
                max_tokens: req.max_tokens,
            })
            .await?;
        let _ = client.shutdown().await;

        Ok(ChatReply {
            content: response.content,
            model_id: response.model_id,
            usage: response.usage.map(|usage| newt_core::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::Caveats;
    use newt_identity::{load_or_generate, plugin_child_metadata, session_root};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    #[test]
    fn new_defaults_envelope_to_none() {
        let b = ProviderPluginBackend::new("openai", "newt-provider-openai", "gpt-4", vec![]);
        assert_eq!(b.agent_key_envelope(), None);
        assert!(!b.has_parent_key(), "new backend must have no parent key");
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

    #[tokio::test]
    async fn complete_round_trips_through_mock_plugin() {
        let dir = tests_common::tempdir();
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"plugin_name":"mock-openai","plugin_version":"0.0.0-test","supported_models":["gpt-test"]}}"#;
        let completion = r#"{"jsonrpc":"2.0","id":2,"result":{"content":"mocked reply","model_id":"gpt-test","usage":{"input_tokens":7,"output_tokens":11}}}"#;
        let shutdown = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
        let plugin = tests_common::mock_plugin_binary(dir.path(), &[init, completion, shutdown]);

        let backend = ProviderPluginBackend::new(
            "openai",
            plugin.to_string_lossy(),
            "gpt-test",
            vec![Tier::Complex],
        );

        let reply = backend
            .complete(ChatRequest::new().user("hello").max_tokens(32))
            .await
            .unwrap();

        assert_eq!(reply.content, "mocked reply");
        assert_eq!(reply.model_id, "gpt-test");
        let usage = reply.usage.expect("usage propagated");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 11);
    }

    #[tokio::test]
    async fn complete_passes_only_whitelisted_environment_variables() {
        let dir = tests_common::tempdir();
        let plugin = env_probe_plugin(dir.path());
        std::env::set_var("NEWT_TEST_OPENAI_ALLOWED", "allowed");
        std::env::set_var("NEWT_TEST_OPENAI_BLOCKED", "blocked");

        let backend = ProviderPluginBackend::new(
            "openai",
            plugin.to_string_lossy(),
            "gpt-test",
            vec![Tier::Complex],
        )
        .with_env_pass(vec!["NEWT_TEST_OPENAI_ALLOWED".into()]);

        let reply = backend
            .complete(ChatRequest::new().user("hello"))
            .await
            .unwrap();

        std::env::remove_var("NEWT_TEST_OPENAI_ALLOWED");
        std::env::remove_var("NEWT_TEST_OPENAI_BLOCKED");
        assert_eq!(reply.content, "allowed=allowed blocked=");
    }

    #[cfg(unix)]
    fn env_probe_plugin(dir: &Path) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("env-probe");
        // Write-then-exec safety (issue #288): create/write/sync/drop
        // explicitly so the write fd is closed before the test spawns
        // this script. Paired with the bounded ETXTBSY-only spawn retry
        // in `PluginClient::spawn_command` — see
        // `tests_common::mock_plugin_binary` for the full rationale.
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(
            br#"#!/bin/sh
IFS= read -r _request || exit 0
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"plugin_name":"env-probe","plugin_version":"0.0.0-test","supported_models":["gpt-test"]}}'
IFS= read -r _request || exit 0
printf '{"jsonrpc":"2.0","id":2,"result":{"content":"allowed=%s blocked=%s","model_id":"gpt-test","usage":null}}\n' "$NEWT_TEST_OPENAI_ALLOWED" "$NEWT_TEST_OPENAI_BLOCKED"
IFS= read -r _request || exit 0
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}'
"#,
        )
        .unwrap();
        file.sync_all().unwrap();
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(windows)]
    fn env_probe_plugin(dir: &Path) -> PathBuf {
        let path = dir.join("env-probe.cmd");
        std::fs::write(
            &path,
            concat!(
                "@echo off\r\n",
                "set /p _request=\r\n",
                "echo {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"plugin_name\":\"env-probe\",\"plugin_version\":\"0.0.0-test\",\"supported_models\":[\"gpt-test\"]}}\r\n",
                "set /p _request=\r\n",
                "echo {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":\"allowed=%NEWT_TEST_OPENAI_ALLOWED% blocked=%NEWT_TEST_OPENAI_BLOCKED%\",\"model_id\":\"gpt-test\",\"usage\":null}}\r\n",
                "set /p _request=\r\n",
                "echo {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\r\n"
            ),
        )
        .unwrap();
        path
    }

    // ── Issue #93: chain-rooting from operator key ────────────────────

    /// `with_parent_key` stores the operator-rooted parent and marks the
    /// backend as ready to mint per-spawn envelopes — i.e. the chain
    /// can be walked back to the operator's `UserKey`.
    #[test]
    fn with_parent_key_stores_arc_and_metadata() {
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let root = Arc::new(session_root(&user));
        let metadata = plugin_child_metadata("provider-plugin", Caveats::top());
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![])
            .with_parent_key(Arc::clone(&root), metadata);
        assert!(b.has_parent_key());
        assert!(
            Arc::ptr_eq(b.parent_key().unwrap(), &root),
            "parent_key must store the exact Arc the caller passed"
        );
    }

    /// `mint_plugin_envelope` produces a base64-encoded JSON cert chain
    /// that, when decoded, verifies and roots back to the operator's
    /// `UserKey`. This is the core #93 invariant: the plugin's leaf
    /// AgentKey chains back to `~/.newt/identity.pem`, NOT a synthetic
    /// `UserKey::generate()` at spawn time.
    #[test]
    fn mint_plugin_envelope_chain_roots_at_operator_userkey() {
        use base64::Engine;
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let user_fp = user.fingerprint();
        let root = Arc::new(session_root(&user));
        let metadata = plugin_child_metadata("provider-plugin", Caveats::top());
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![])
            .with_parent_key(root, metadata);

        let envelope = b
            .mint_plugin_envelope()
            .expect("parent_key set → envelope must be available")
            .expect("delegation must succeed for ⊑ caveats");

        let json = base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .expect("envelope is base64");
        let cert: agent_mesh_protocol::CertChain =
            serde_json::from_slice(&json).expect("envelope is JSON CertChain");
        cert.verify().expect("chain must verify end to end");
        assert_eq!(
            cert.user_fingerprint(),
            user_fp,
            "leaf must chain back to the operator UserKey"
        );
    }

    /// `mint_plugin_envelope` returns `None` when no parent key is
    /// configured — the caller falls back to the legacy
    /// `agent_key_envelope` path.
    #[test]
    fn mint_plugin_envelope_none_without_parent_key() {
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![]);
        assert!(
            b.mint_plugin_envelope().is_none(),
            "no parent key → no envelope minted"
        );
    }

    /// `with_parent_key` followed by a request to amplify authority
    /// surfaces as `EnvelopeError::Amplification` on `mint_plugin_envelope`
    /// — never as a panic, and never as a silent downgrade.
    #[test]
    fn mint_plugin_envelope_refuses_amplification() {
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let root = session_root(&user);
        // Narrow the parent's authority first.
        let narrowed_caveats = Caveats {
            exec: newt_core::Scope::none(),
            ..Caveats::top()
        };
        let worker = newt_identity::attenuate(&root, &narrowed_caveats).unwrap();
        // Now ask the plugin to run with strictly more (⊤ exec).
        let plugin_meta = plugin_child_metadata("evil-plugin", Caveats::top());
        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![])
            .with_parent_key(Arc::new(worker), plugin_meta);

        let err = b
            .mint_plugin_envelope()
            .expect("parent set → envelope path attempted")
            .expect_err("amplifying delegation must refuse");
        assert!(
            matches!(err, newt_identity::EnvelopeError::Amplification),
            "expected Amplification, got {err:?}"
        );
    }

    /// When both `parent_key` and `agent_key_envelope` are set, the
    /// parent-key path wins — chain-rooted minting takes precedence
    /// over a hand-crafted opaque envelope. This is the #93 discipline:
    /// a configured parent must govern, even if a legacy envelope was
    /// also stored.
    #[test]
    fn parent_key_takes_precedence_over_opaque_envelope() {
        use base64::Engine;
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let root = Arc::new(session_root(&user));
        let metadata = plugin_child_metadata("plugin", Caveats::top());

        let b = ProviderPluginBackend::new("openai", "x", "gpt-4", vec![])
            .with_agent_key_envelope("opaque-legacy-string-should-be-ignored")
            .with_parent_key(Arc::clone(&root), metadata);

        let envelope = b
            .mint_plugin_envelope()
            .expect("parent_key set → minted envelope path wins")
            .expect("delegation must succeed");
        // The minted envelope is real base64 JSON, not the opaque string.
        assert!(base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .is_ok());
        assert_ne!(envelope, "opaque-legacy-string-should-be-ignored");
    }

    /// The three-link operator → worker → plugin chain verifies end to
    /// end through `spawn_command`'s mint path. Models the real
    /// threading: WorkerIdentity::Operator { root } → attenuate per
    /// dispatch → delegate to plugin.
    #[test]
    fn three_link_chain_operator_worker_plugin_verifies() {
        use base64::Engine;
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let root = session_root(&user);
        // Worker key (link 2): a real attenuated dispatch key.
        let dispatch_caveats = Caveats {
            exec: newt_core::Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let worker = newt_identity::attenuate(&root, &dispatch_caveats).unwrap();
        // Plugin key (link 3): further-narrowed to read-only.
        let plugin_caveats = Caveats {
            fs_write: newt_core::Scope::none(),
            exec: newt_core::Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let plugin_meta = plugin_child_metadata("plugin", plugin_caveats.clone());
        let b = ProviderPluginBackend::new("p", "x", "m", vec![])
            .with_parent_key(Arc::new(worker), plugin_meta);

        let envelope = b.mint_plugin_envelope().unwrap().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .unwrap();
        let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&bytes).unwrap();
        leaf.verify().unwrap();
        // Walk to the third link: it must be Issuer::User and match
        // the operator's pubkey.
        assert_eq!(leaf.user_fingerprint(), user.fingerprint());
        assert_eq!(leaf.metadata.caveats, plugin_caveats);
    }

    /// Each call to `mint_plugin_envelope` mints a *fresh* delegated
    /// child — the ephemeral leaf signing key changes per spawn, even
    /// though the parent key is stable. This is the right behavior:
    /// agent keys are per-process ephemeral, and re-using the same
    /// child key across spawns would couple plugin processes that
    /// should be isolated.
    #[test]
    fn each_spawn_mints_a_fresh_child_leaf_key() {
        let dir = TempDir::new().unwrap();
        let user = load_or_generate(&dir.path().join("identity.pem")).unwrap();
        let root = Arc::new(session_root(&user));
        let metadata = plugin_child_metadata("plugin", Caveats::top());
        let b = ProviderPluginBackend::new("p", "x", "m", vec![]).with_parent_key(root, metadata);

        let e1 = b.mint_plugin_envelope().unwrap().unwrap();
        let e2 = b.mint_plugin_envelope().unwrap().unwrap();
        // The envelopes carry distinct ephemeral leaf keys, so the
        // base64 bytes differ — proving we don't cache and reuse a
        // single per-spawn child.
        assert_ne!(e1, e2, "every spawn must mint a fresh ephemeral child leaf");
    }
}
