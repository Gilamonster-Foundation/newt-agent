# Feature Proposal: Unified Capability Registry (Kit System)

> **Status (2026-08-16): Draft — partly superseded by existing code.** A "kit" already
> exists: `newt-core/src/kit.rs` (`Axis`, `Tier`, `RegistryEntry`, `component()`) and
> `Loadout.kit` naming a `[bundles.*]` in `newt-core/src/config.rs`; `KitPermissions` reduces to
> the existing `Caveats` / `PermissionGate` / `ExposureProfile`. This doc therefore reduces to
> "extend `BundleConfig` for role/permission scoping" (roadmap step A3) — no `newt-kit` crate.
> See the reconciliation table in [companion-roadmap.md](companion-roadmap.md).

## Overview

Introduce a **Kit System** — a unified, typed registry for all extensibility surfaces in the agent runtime: skills, tools, MCP servers, plugins, and future capability types. Each "kit" declares its capabilities, permissions, and remote-callable policies in a single manifest.

## Motivation

Today, Newt has multiple parallel extension mechanisms:
- **Skills** (`newt-skills`) — behavioral packages with prompts, tools, hooks
- **Tools** (`newt-tools`) — individual function implementations
- **MCP Servers** (`newt-mcp-server`) — Model Context Protocol endpoints
- **Plugins** (`plugins-protocol`) — external process communication

Each has its own registration, discovery, and permission model. A Kit System unifies these under one abstraction.

## Design

### Kit Manifest

```rust
// newt-kit/src/manifest.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KitManifest {
    /// Stable identifier (e.g., "github.com/org/kit-name")
    pub id: KitId,
    pub version: semver::Version,
    pub metadata: KitMetadata,
    pub capabilities: KitCapabilities,
    pub permissions: KitPermissions,
    pub remote_policy: RemoteCallablePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KitMetadata {
    pub name: String,
    pub description: String,
    pub author: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KitCapabilities {
    /// Skill definitions (prompts, hooks, sub-agents)
    pub skills: Vec<SkillDef>,
    /// Tool implementations
    pub tools: Vec<ToolDef>,
    /// MCP server configurations
    pub mcp_servers: Vec<McpServerDef>,
    /// Plugin protocols (external processes)
    pub plugins: Vec<PluginDef>,
    /// Custom capability extensions
    pub custom: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// NOTE: superseded — reuse `newt_core::caveats::Caveats` + `ExposureProfile`
// (docs/design/tool-exposure-controller.md) instead of a new permissions type.
pub struct KitPermissions {
    /// Filesystem access patterns
    pub fs: FsPermissionSet,
    /// Network access (hosts, ports)
    pub network: NetworkPermissionSet,
    /// Process execution
    pub exec: ExecPermissionSet,
    /// MCP server access
    pub mcp: McpPermissionSet,
    /// Inter-kit communication
    pub kit_calls: Vec<KitId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum RemoteCallablePolicy {
    /// Never callable from remote agents
    LocalOnly,
    /// Callable with explicit allowlist
    AllowList { peers: Vec<PeerId> },
    /// Callable by any authenticated peer
    Authenticated,
    /// Callable by anyone (public)
    Public,
}
```

### Kit Registry

```rust
// newt-kit/src/registry.rs
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct KitRegistry {
    kits: RwLock<HashMap<KitId, Arc<LoadedKit>>>,
    resolver: KitResolver,
}

pub struct LoadedKit {
    pub manifest: KitManifest,
    pub instance: KitInstance,
    pub state: KitState,
}

pub enum KitInstance {
    Builtin(Arc<dyn BuiltinKit>),
    Dynamic(DynamicKitHandle),
    Remote(RemoteKitProxy),
}

impl KitRegistry {
    pub async fn load(&self, source: KitSource) -> Result<KitId, KitError>;
    pub async fn unload(&self, id: &KitId) -> Result<(), KitError>;
    pub async fn get(&self, id: &KitId) -> Option<Arc<LoadedKit>>;
    pub async fn query(&self, query: KitQuery) -> Vec<Arc<LoadedKit>>;
    pub async fn call(&self, call: KitCall) -> Result<KitCallResult, KitError>;
}
```

### Integration Points

| Surface | Current Location | Kit Integration |
|---------|------------------|-----------------|
| Skills | `newt-skills` | `KitCapabilities.skills` |
| Tools | `newt-tools` | `KitCapabilities.tools` |
| MCP | `newt-mcp-server` / `newt-mcp-client` | `KitCapabilities.mcp_servers` |
| Plugins | `plugins-protocol` | `KitCapabilities.plugins` |
| Mesh | `newt-mesh` | `RemoteCallablePolicy` + `kit_calls` permissions |

## Implementation Phases

### Phase 1: Core Crate (`newt-kit`)
- Manifest schema + validation
- In-memory registry with load/unload/query
- Permission evaluation engine
- Builtin kit trait (dynamic loading via dlopen / wasm moved to Open Questions)

### Phase 2: Migration Adapters
- `newt-skills` → kit adapter
- `newt-tools` → kit adapter
- `newt-mcp-*` → kit adapter
- `plugins-protocol` → kit adapter

### Phase 3: Remote & Mesh
- `RemoteCallablePolicy` enforcement in `newt-mesh`
- Kit discovery over mesh
- Cross-agent kit calls with auth

### Phase 4: Developer Experience
- `newt kit` CLI commands
- Kit publishing format (tarball + manifest)
- Local kit development workflow

## Benefits for Gilamonster-Agent

- **Module = Kit**: Each agent in the matrix loads its own kit set with isolated permissions
- **Matrix-wide discovery**: `KitRegistry::query()` spans the mesh
- **Policy uniformity**: One permission model for local skills, remote MCP, peer plugins
- **Hot reload**: Kit unload/load without agent restart

## Related Work

- VS Code extension manifest + capability model
- WASI component model (wit bindings)

## Open Questions

0. **Dynamic loading**: `dlopen`/WASM is deferred entirely; process/MCP boundaries first.
   `KitKind::Speech` is referenced by speech-pipeline.md but not defined here — add it when B1 lands.

1. **WASM vs native plugins**: Start with native `dlopen`, add WASM component model later?
2. **Kit versioning**: Semver + compatibility matrix, or lockfile-based?
3. **Sandboxing**: Process isolation (current plugin model) vs in-process with capabilities?