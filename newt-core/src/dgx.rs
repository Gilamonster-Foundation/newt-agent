//! NVIDIA DGX endpoint configuration — the data model behind the
//! `newt dgx` command suite.
//!
//! A [`DgxConfig`] describes one or more DGX *nodes*, each exposing up to four
//! named *endpoints*, a set of saved *formations* (model + endpoint presets),
//! and the currently-active selection. It is an optional sub-table of
//! [`crate::Config`] (`[dgx]` in `newt.toml`); when absent, newt never contacts
//! a DGX host unless a `NEWT_DGX_*` environment variable is set — there are no
//! leaky defaults.
//!
//! # Endpoint flavors ([`EndpointKind`])
//!
//! | kind | config key | example URL |
//! |------|-----------|-------------|
//! | direct DGX (Traefik) | `ollama` | `https://REDACTED-HOST` |
//! | round-robin LB | `ollama_lb` | `https://REDACTED-HOST` |
//! | in-cluster proxy | `in_cluster` | `http://ollama-proxy.inference.svc.cluster.local:11434` |
//! | vLLM (OpenAI-compatible) | `vllm` | `http://REDACTED-HOST:8000` |
//!
//! The first three speak Ollama's native `/api` surface; only `vllm` is
//! OpenAI-compatible (`/v1`). HTTPS endpoints (the Traefik-fronted hosts on
//! port 443) are supported directly — give the full `https://...` URL with no
//! port.
//!
//! # Environment overrides
//!
//! Resolution consults the environment before the config file, so an
//! unconfigured install can still target a DGX:
//!
//! - `NEWT_DGX_OLLAMA_URL`, `NEWT_DGX_OLLAMA_LB_URL`,
//!   `NEWT_DGX_IN_CLUSTER_URL`, `NEWT_DGX_VLLM_URL` — full URL for one flavor.
//! - `NEWT_DGX_HOST` (plus optional `NEWT_DGX_SCHEME`, `NEWT_DGX_OLLAMA_PORT`,
//!   `NEWT_DGX_VLLM_PORT`) — synthesize the `ollama`/`vllm` URL from a bare
//!   host when no explicit URL is set. The LB and in-cluster proxy are distinct
//!   hostnames, not ports on `NEWT_DGX_HOST`, so they are never synthesized.
//! - `NEWT_DGX_MODEL` — active model id.
//! - `NEWT_DGX_SSH_HOST`, `NEWT_DGX_SSH_USER` — SSH target for `run`/`push`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Endpoint kind
// ---------------------------------------------------------------------------

/// The four endpoint flavors a DGX node can expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// Direct DGX Ollama endpoint (typically Traefik-fronted HTTPS).
    #[default]
    Ollama,
    /// Round-robin load balancer across Ollama backends.
    OllamaLb,
    /// In-cluster, model-aware Ollama proxy (for pods).
    InCluster,
    /// vLLM OpenAI-compatible endpoint.
    Vllm,
}

impl EndpointKind {
    /// Every endpoint kind, in display order.
    pub const ALL: [Self; 4] = [Self::Ollama, Self::OllamaLb, Self::InCluster, Self::Vllm];

    /// The TOML config key / CLI token for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OllamaLb => "ollama_lb",
            Self::InCluster => "in_cluster",
            Self::Vllm => "vllm",
        }
    }

    /// The environment variable that overrides this flavor's full URL.
    pub fn url_env_var(self) -> &'static str {
        match self {
            Self::Ollama => "NEWT_DGX_OLLAMA_URL",
            Self::OllamaLb => "NEWT_DGX_OLLAMA_LB_URL",
            Self::InCluster => "NEWT_DGX_IN_CLUSTER_URL",
            Self::Vllm => "NEWT_DGX_VLLM_URL",
        }
    }

    /// Whether this flavor speaks the OpenAI-compatible (`/v1`) API. Only
    /// `vllm` does; the rest speak Ollama's native `/api`.
    pub fn is_openai_compatible(self) -> bool {
        matches!(self, Self::Vllm)
    }
}

impl fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EndpointKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase().replace('-', "_");
        match norm.as_str() {
            "ollama" => Ok(Self::Ollama),
            "ollama_lb" | "lb" => Ok(Self::OllamaLb),
            "in_cluster" | "incluster" | "cluster" | "proxy" => Ok(Self::InCluster),
            "vllm" => Ok(Self::Vllm),
            other => Err(format!(
                "unknown endpoint kind {other:?} (expected one of: ollama, ollama_lb, in_cluster, vllm)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a DGX endpoint / model / host could not be resolved. Every variant is
/// actionable: it names what is missing and how to set it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DgxNotConfigured {
    /// No nodes in config and no `NEWT_DGX_*` fallback.
    #[error("no DGX nodes configured — run `newt dgx setup` or set NEWT_DGX_HOST")]
    NoNodes,

    /// Several nodes exist but none is marked active.
    #[error("no active DGX node selected ({count} configured) — set [dgx].active_node or run `newt dgx node use <name>`")]
    NoActiveNode { count: usize },

    /// `active_node` names a node that isn't in the list.
    #[error("DGX node {name:?} not found in config")]
    NodeNotFound { name: String },

    /// The active node has no URL for the requested flavor.
    #[error("DGX node {node:?} has no {kind} endpoint set")]
    EndpointUnset { node: String, kind: EndpointKind },

    /// No active model and no `NEWT_DGX_MODEL`.
    #[error("no active DGX model — set [dgx].active_model, NEWT_DGX_MODEL, or run `newt dgx use <model>`")]
    NoActiveModel,

    /// No SSH host for `run`/`push`.
    #[error("DGX node {node:?} has no ssh_host — set it in config or via NEWT_DGX_SSH_HOST")]
    NoSshHost { node: String },
}

// ---------------------------------------------------------------------------
// Node / formation
// ---------------------------------------------------------------------------

/// A single DGX host and the endpoints it exposes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DgxNode {
    /// Short name used by `newt dgx node use <name>`.
    pub name: String,

    /// Direct DGX Ollama URL (e.g. `https://REDACTED-HOST`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama: Option<String>,

    /// Round-robin LB URL (e.g. `https://REDACTED-HOST`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama_lb: Option<String>,

    /// In-cluster proxy URL (plain HTTP cluster DNS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_cluster: Option<String>,

    /// vLLM OpenAI-compatible URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vllm: Option<String>,

    /// SSH host for `run`/`push` (may differ from the HTTP hostnames).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,

    /// SSH user; falls back to `$USER`, then `dgx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_user: Option<String>,
}

impl DgxNode {
    /// The configured URL for `kind` on this node, if any.
    pub fn endpoint(&self, kind: EndpointKind) -> Option<&str> {
        match kind {
            EndpointKind::Ollama => self.ollama.as_deref(),
            EndpointKind::OllamaLb => self.ollama_lb.as_deref(),
            EndpointKind::InCluster => self.in_cluster.as_deref(),
            EndpointKind::Vllm => self.vllm.as_deref(),
        }
    }
}

/// A named (model, endpoint) preset — `newt dgx formation <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DgxFormation {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: EndpointKind,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// The `[dgx]` sub-table: nodes, formations, and the active selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DgxConfig {
    /// Name of the active node; `None` means "the only node, if unambiguous".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_node: Option<String>,

    /// Active endpoint flavor (defaults to `ollama`).
    pub active_endpoint: EndpointKind,

    /// Active model id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_model: Option<String>,

    /// Configured DGX nodes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<DgxNode>,

    /// Saved (model, endpoint) presets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub formations: Vec<DgxFormation>,
}

impl DgxConfig {
    /// Look up a node by name.
    pub fn node(&self, name: &str) -> Option<&DgxNode> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// Look up a formation by name.
    pub fn formation(&self, name: &str) -> Option<&DgxFormation> {
        self.formations.iter().find(|f| f.name == name)
    }

    /// Resolve the active node: the one named by `active_node`, or — if that is
    /// unset — the single configured node when there is exactly one.
    pub fn active_node(&self) -> Result<&DgxNode, DgxNotConfigured> {
        match &self.active_node {
            Some(name) => self
                .node(name)
                .ok_or_else(|| DgxNotConfigured::NodeNotFound { name: name.clone() }),
            None => match self.nodes.as_slice() {
                [] => Err(DgxNotConfigured::NoNodes),
                [only] => Ok(only),
                many => Err(DgxNotConfigured::NoActiveNode { count: many.len() }),
            },
        }
    }

    /// Resolve the URL for the *active* endpoint flavor.
    pub fn resolve_endpoint(&self) -> Result<String, DgxNotConfigured> {
        self.resolve_endpoint_for(self.active_endpoint)
    }

    /// Resolve the URL for a specific endpoint flavor, consulting the real
    /// process environment.
    pub fn resolve_endpoint_for(&self, kind: EndpointKind) -> Result<String, DgxNotConfigured> {
        self.resolve_endpoint_for_with(kind, &|k| std::env::var(k).ok())
    }

    /// [`resolve_endpoint_for`](Self::resolve_endpoint_for) with an injectable
    /// environment lookup (for testing). Precedence: per-flavor URL env var →
    /// configured node URL → `NEWT_DGX_HOST` synthesis (ollama/vllm only).
    pub fn resolve_endpoint_for_with(
        &self,
        kind: EndpointKind,
        get_env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, DgxNotConfigured> {
        if let Some(url) = nonempty(get_env(kind.url_env_var())) {
            return Ok(url);
        }
        if let Ok(node) = self.active_node() {
            if let Some(url) = node.endpoint(kind) {
                return Ok(url.to_string());
            }
        }
        if let Some(url) = synth_from_host(kind, get_env) {
            return Ok(url);
        }
        match self.active_node() {
            Ok(node) => Err(DgxNotConfigured::EndpointUnset {
                node: node.name.clone(),
                kind,
            }),
            Err(e) => Err(e),
        }
    }

    /// Resolve the active model id (env `NEWT_DGX_MODEL` wins).
    pub fn resolve_active_model(&self) -> Result<String, DgxNotConfigured> {
        self.resolve_active_model_with(&|k| std::env::var(k).ok())
    }

    /// [`resolve_active_model`](Self::resolve_active_model) with injectable env.
    pub fn resolve_active_model_with(
        &self,
        get_env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, DgxNotConfigured> {
        if let Some(model) = nonempty(get_env("NEWT_DGX_MODEL")) {
            return Ok(model);
        }
        self.active_model
            .clone()
            .ok_or(DgxNotConfigured::NoActiveModel)
    }

    /// Resolve the SSH host (env `NEWT_DGX_SSH_HOST` wins).
    pub fn ssh_host(&self) -> Result<String, DgxNotConfigured> {
        self.ssh_host_with(&|k| std::env::var(k).ok())
    }

    /// [`ssh_host`](Self::ssh_host) with injectable env.
    pub fn ssh_host_with(
        &self,
        get_env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, DgxNotConfigured> {
        if let Some(host) = nonempty(get_env("NEWT_DGX_SSH_HOST")) {
            return Ok(host);
        }
        let node = self.active_node()?;
        node.ssh_host
            .clone()
            .ok_or_else(|| DgxNotConfigured::NoSshHost {
                node: node.name.clone(),
            })
    }

    /// Resolve the SSH user: `NEWT_DGX_SSH_USER` → node `ssh_user` → `$USER`
    /// → `dgx`. Never fails.
    pub fn ssh_user(&self) -> String {
        self.ssh_user_with(&|k| std::env::var(k).ok())
    }

    /// [`ssh_user`](Self::ssh_user) with injectable env.
    pub fn ssh_user_with(&self, get_env: &dyn Fn(&str) -> Option<String>) -> String {
        if let Some(user) = nonempty(get_env("NEWT_DGX_SSH_USER")) {
            return user;
        }
        if let Ok(node) = self.active_node() {
            if let Some(user) = &node.ssh_user {
                return user.clone();
            }
        }
        nonempty(get_env("USER")).unwrap_or_else(|| "dgx".to_string())
    }

    /// A ready-to-write template for the reference `home.lab` DGX topology.
    /// `newt dgx setup` offers this as the suggested starting point; it is
    /// never applied automatically.
    pub fn home_template() -> Self {
        Self {
            active_node: Some("home".to_string()),
            active_endpoint: EndpointKind::Ollama,
            active_model: Some("qwen2.5-coder:32b".to_string()),
            nodes: vec![DgxNode {
                name: "home".to_string(),
                ollama: Some("https://REDACTED-HOST".to_string()),
                ollama_lb: Some("https://REDACTED-HOST".to_string()),
                in_cluster: Some(
                    "http://ollama-proxy.inference.svc.cluster.local:11434".to_string(),
                ),
                vllm: None,
                ssh_host: Some("REDACTED-HOST".to_string()),
                ssh_user: None,
            }],
            formations: vec![
                DgxFormation {
                    name: "coding".to_string(),
                    model: "qwen2.5-coder:32b".to_string(),
                    endpoint: EndpointKind::Ollama,
                },
                DgxFormation {
                    name: "review".to_string(),
                    model: "llama3.1:70b".to_string(),
                    endpoint: EndpointKind::InCluster,
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Treat empty / whitespace-only values as unset.
fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

/// Synthesize an `ollama`/`vllm` URL from `NEWT_DGX_HOST` + optional scheme and
/// port. Returns `None` for `ollama_lb` / `in_cluster` (distinct hosts).
fn synth_from_host(kind: EndpointKind, get_env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let host = nonempty(get_env("NEWT_DGX_HOST"))?;
    let (port_var, default_port) = match kind {
        EndpointKind::Ollama => ("NEWT_DGX_OLLAMA_PORT", "11434"),
        EndpointKind::Vllm => ("NEWT_DGX_VLLM_PORT", "8000"),
        EndpointKind::OllamaLb | EndpointKind::InCluster => return None,
    };
    let scheme = nonempty(get_env("NEWT_DGX_SCHEME")).unwrap_or_else(|| "http".to_string());
    let port = nonempty(get_env(port_var)).unwrap_or_else(|| default_port.to_string());
    Some(format!("{scheme}://{host}:{port}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an injectable env lookup from a fixed set of pairs.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    // --- EndpointKind --------------------------------------------------

    #[test]
    fn endpoint_kind_str_roundtrip() {
        for kind in EndpointKind::ALL {
            let s = kind.as_str();
            assert_eq!(s.parse::<EndpointKind>().unwrap(), kind);
            assert_eq!(kind.to_string(), s);
        }
    }

    #[test]
    fn endpoint_kind_from_str_aliases() {
        assert_eq!(
            "LB".parse::<EndpointKind>().unwrap(),
            EndpointKind::OllamaLb
        );
        assert_eq!(
            "in-cluster".parse::<EndpointKind>().unwrap(),
            EndpointKind::InCluster
        );
        assert_eq!(
            "proxy".parse::<EndpointKind>().unwrap(),
            EndpointKind::InCluster
        );
        assert_eq!(
            "  Ollama ".parse::<EndpointKind>().unwrap(),
            EndpointKind::Ollama
        );
    }

    #[test]
    fn endpoint_kind_from_str_rejects_unknown() {
        let err = "bogus".parse::<EndpointKind>().unwrap_err();
        assert!(
            err.contains("bogus"),
            "error should name the bad input: {err}"
        );
    }

    #[test]
    fn endpoint_kind_default_is_ollama() {
        assert_eq!(EndpointKind::default(), EndpointKind::Ollama);
    }

    #[test]
    fn endpoint_kind_openai_compat_only_vllm() {
        assert!(EndpointKind::Vllm.is_openai_compatible());
        assert!(!EndpointKind::Ollama.is_openai_compatible());
        assert!(!EndpointKind::InCluster.is_openai_compatible());
    }

    #[test]
    fn endpoint_kind_serde_snake_case() {
        let json = serde_json::to_string(&EndpointKind::InCluster).unwrap();
        assert_eq!(json, "\"in_cluster\"");
        let back: EndpointKind = serde_json::from_str("\"ollama_lb\"").unwrap();
        assert_eq!(back, EndpointKind::OllamaLb);
    }

    // --- node accessor -------------------------------------------------

    #[test]
    fn node_endpoint_accessor() {
        let node = DgxNode {
            name: "n".into(),
            ollama: Some("o".into()),
            vllm: Some("v".into()),
            ..Default::default()
        };
        assert_eq!(node.endpoint(EndpointKind::Ollama), Some("o"));
        assert_eq!(node.endpoint(EndpointKind::Vllm), Some("v"));
        assert_eq!(node.endpoint(EndpointKind::OllamaLb), None);
        assert_eq!(node.endpoint(EndpointKind::InCluster), None);
    }

    // --- active_node ---------------------------------------------------

    #[test]
    fn active_node_single_is_implicit() {
        let cfg = DgxConfig {
            nodes: vec![DgxNode {
                name: "solo".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(cfg.active_node().unwrap().name, "solo");
    }

    #[test]
    fn active_node_explicit_selection() {
        let cfg = DgxConfig {
            active_node: Some("b".into()),
            nodes: vec![
                DgxNode {
                    name: "a".into(),
                    ..Default::default()
                },
                DgxNode {
                    name: "b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(cfg.active_node().unwrap().name, "b");
    }

    #[test]
    fn active_node_unknown_name_errors() {
        let cfg = DgxConfig {
            active_node: Some("ghost".into()),
            nodes: vec![DgxNode {
                name: "real".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cfg.active_node().unwrap_err(),
            DgxNotConfigured::NodeNotFound {
                name: "ghost".into()
            }
        );
    }

    #[test]
    fn active_node_zero_nodes_errors() {
        let cfg = DgxConfig::default();
        assert_eq!(cfg.active_node().unwrap_err(), DgxNotConfigured::NoNodes);
    }

    #[test]
    fn active_node_many_without_active_errors() {
        let cfg = DgxConfig {
            nodes: vec![
                DgxNode {
                    name: "a".into(),
                    ..Default::default()
                },
                DgxNode {
                    name: "b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            cfg.active_node().unwrap_err(),
            DgxNotConfigured::NoActiveNode { count: 2 }
        );
    }

    // --- endpoint resolution -------------------------------------------

    #[test]
    fn resolve_endpoint_uses_config_url() {
        let cfg = DgxConfig::home_template();
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Ollama, &env_of(&[]))
                .unwrap(),
            "https://REDACTED-HOST"
        );
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::InCluster, &env_of(&[]))
                .unwrap(),
            "http://ollama-proxy.inference.svc.cluster.local:11434"
        );
    }

    #[test]
    fn resolve_active_endpoint_default_is_ollama() {
        let cfg = DgxConfig::home_template();
        assert_eq!(
            cfg.resolve_endpoint_for_with(cfg.active_endpoint, &env_of(&[]))
                .unwrap(),
            "https://REDACTED-HOST"
        );
    }

    #[test]
    fn resolve_endpoint_env_url_wins() {
        let cfg = DgxConfig::home_template();
        let env = env_of(&[("NEWT_DGX_OLLAMA_URL", "http://override:1234")]);
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Ollama, &env)
                .unwrap(),
            "http://override:1234"
        );
    }

    #[test]
    fn resolve_endpoint_empty_env_url_ignored() {
        let cfg = DgxConfig::home_template();
        let env = env_of(&[("NEWT_DGX_OLLAMA_URL", "   ")]);
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Ollama, &env)
                .unwrap(),
            "https://REDACTED-HOST"
        );
    }

    #[test]
    fn resolve_endpoint_host_synthesis_ollama_default_port() {
        let cfg = DgxConfig::default();
        let env = env_of(&[("NEWT_DGX_HOST", "dgx.example")]);
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Ollama, &env)
                .unwrap(),
            "http://dgx.example:11434"
        );
    }

    #[test]
    fn resolve_endpoint_host_synthesis_vllm_scheme_and_port() {
        let cfg = DgxConfig::default();
        let env = env_of(&[
            ("NEWT_DGX_HOST", "dgx.example"),
            ("NEWT_DGX_SCHEME", "https"),
            ("NEWT_DGX_VLLM_PORT", "8443"),
        ]);
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Vllm, &env)
                .unwrap(),
            "https://dgx.example:8443"
        );
    }

    #[test]
    fn resolve_endpoint_host_synthesis_skips_lb_and_cluster() {
        let cfg = DgxConfig::default();
        let env = env_of(&[("NEWT_DGX_HOST", "dgx.example")]);
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::OllamaLb, &env)
                .unwrap_err(),
            DgxNotConfigured::NoNodes
        );
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::InCluster, &env)
                .unwrap_err(),
            DgxNotConfigured::NoNodes
        );
    }

    #[test]
    fn resolve_endpoint_unset_on_node_errors() {
        let cfg = DgxConfig {
            nodes: vec![DgxNode {
                name: "n".into(),
                ollama: Some("o".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Vllm, &env_of(&[]))
                .unwrap_err(),
            DgxNotConfigured::EndpointUnset {
                node: "n".into(),
                kind: EndpointKind::Vllm
            }
        );
    }

    #[test]
    fn resolve_endpoint_no_config_no_env_errors() {
        let cfg = DgxConfig::default();
        assert_eq!(
            cfg.resolve_endpoint_for_with(EndpointKind::Ollama, &env_of(&[]))
                .unwrap_err(),
            DgxNotConfigured::NoNodes
        );
    }

    // --- model / ssh ---------------------------------------------------

    #[test]
    fn resolve_active_model_config_then_env() {
        let cfg = DgxConfig::home_template();
        assert_eq!(
            cfg.resolve_active_model_with(&env_of(&[])).unwrap(),
            "qwen2.5-coder:32b"
        );
        let env = env_of(&[("NEWT_DGX_MODEL", "llama3.1:70b")]);
        assert_eq!(cfg.resolve_active_model_with(&env).unwrap(), "llama3.1:70b");
    }

    #[test]
    fn resolve_active_model_missing_errors() {
        let cfg = DgxConfig::default();
        assert_eq!(
            cfg.resolve_active_model_with(&env_of(&[])).unwrap_err(),
            DgxNotConfigured::NoActiveModel
        );
    }

    #[test]
    fn ssh_host_config_then_env() {
        let cfg = DgxConfig::home_template();
        assert_eq!(cfg.ssh_host_with(&env_of(&[])).unwrap(), "REDACTED-HOST");
        let env = env_of(&[("NEWT_DGX_SSH_HOST", "other.host")]);
        assert_eq!(cfg.ssh_host_with(&env).unwrap(), "other.host");
    }

    #[test]
    fn ssh_host_missing_errors() {
        let cfg = DgxConfig {
            nodes: vec![DgxNode {
                name: "n".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cfg.ssh_host_with(&env_of(&[])).unwrap_err(),
            DgxNotConfigured::NoSshHost { node: "n".into() }
        );
    }

    #[test]
    fn ssh_user_precedence_chain() {
        let cfg = DgxConfig {
            nodes: vec![DgxNode {
                name: "n".into(),
                ssh_user: Some("nodeuser".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cfg.ssh_user_with(&env_of(&[("NEWT_DGX_SSH_USER", "envuser")])),
            "envuser"
        );
        assert_eq!(cfg.ssh_user_with(&env_of(&[])), "nodeuser");

        let bare = DgxConfig {
            nodes: vec![DgxNode {
                name: "n".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(bare.ssh_user_with(&env_of(&[("USER", "shell")])), "shell");
        assert_eq!(bare.ssh_user_with(&env_of(&[])), "dgx");
    }

    // --- home template + round-trip ------------------------------------

    #[test]
    fn home_template_resolves() {
        let cfg = DgxConfig::home_template();
        assert_eq!(cfg.active_node().unwrap().name, "home");
        let url = cfg
            .resolve_endpoint_for_with(EndpointKind::Ollama, &env_of(&[]))
            .unwrap();
        assert!(
            url.starts_with("https://"),
            "direct DGX should be https: {url}"
        );
        assert!(
            !url.contains(":11434"),
            "Traefik endpoint should not carry :11434: {url}"
        );
        assert!(cfg.formation("coding").is_some());
        assert!(cfg.formation("missing").is_none());
    }

    #[test]
    fn dgx_config_toml_roundtrip() {
        let cfg = DgxConfig::home_template();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: DgxConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn real_env_wrappers_smoke() {
        // Exercise the public, real-environment wrappers. home_template's
        // values come from config, so these succeed regardless of the ambient
        // environment (an env override would only change the value).
        let cfg = DgxConfig::home_template();
        assert!(cfg.resolve_endpoint().is_ok());
        assert!(cfg.resolve_endpoint_for(EndpointKind::InCluster).is_ok());
        assert!(cfg.resolve_active_model().is_ok());
        assert!(cfg.ssh_host().is_ok());
        assert!(!cfg.ssh_user().is_empty());
    }
}
