# Feature Proposal: Agent Module Isolation (Module Scopes)

> **Status (2026-08-16): Draft — partly superseded by existing code.** Per-agent scoping
> already exists piecemeal: `RoleProfile` (`role_profile.rs`), loadouts / `BundleConfig`,
> `TabSidecar` (`newt-tui/src/tabs.rs`), `CrewRunner`, and `send_budget.rs`. Permissions are
> `Caveats` / `PermissionGate`. **The Rust below is non-compiling pseudocode** (it clones a
> `oneshot::Sender`, `.await`s in sync fns, and reads a phantom `self.permissions`) — treat it as
> intent, not API. See the reconciliation table in [companion-roadmap.md](companion-roadmap.md).

## Overview

Introduce **Module Scopes** — first-class isolation boundaries for agent instances. Each agent runs in a "module" with its own kit registry view, permission set, resource limits, and lifecycle hooks. Modules can be composed hierarchically (parent/child) and communicate via typed message passing.

## Motivation

Gilamonster's multi-agent matrix runs many agents in a single process (or across processes). Today there's no clean boundary:
- Skills/tools leak across agents
- Permission model is global
- No structured lifecycle for agent spawn/termination
- Resource accounting (tokens, API calls, memory) is per-process, not per-agent

Module Scopes solve this by making "agent = module" a runtime primitive.

## Design

### Module Definition

```rust
// newt-module/src/module.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSpec {
    pub id: ModuleId,
    pub name: String,
    pub parent: Option<ModuleId>,
    pub kit_scope: KitScope,
    pub permissions: ModulePermissions,
    pub resources: ResourceLimits,
    pub lifecycle: LifecycleHooks,
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KitScope {
    /// Kits explicitly available to this module
    pub allowed_kits: Vec<KitId>,
    /// Kits explicitly denied (override allowed)
    pub denied_kits: Vec<KitId>,
    /// Inherit parent's kit scope?
    pub inherit_parent: bool,
    /// Auto-discover kits matching tags?
    pub auto_discover_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModulePermissions {
    /// Filesystem sandbox root (None = inherit parent)
    pub fs_root: Option<PathBuf>,
    /// Network allowlist
    pub network: NetworkPolicy,
    /// Process execution policy
    pub exec: ExecPolicy,
    /// Token budget per window
    pub token_budget: Option<TokenBudget>,
    /// API call budget per window
    pub api_budget: Option<ApiBudget>,
    /// Custom capability gates
    pub capabilities: HashMap<String, CapabilityPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: Option<u64>,
    pub max_cpu_percent: Option<f32>,
    pub max_concurrent_tasks: Option<u32>,
    pub max_kit_instances: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleHooks {
    pub on_spawn: Vec<HookDef>,
    pub on_ready: Vec<HookDef>,
    pub on_shutdown: Vec<HookDef>,
    pub on_error: Vec<HookDef>,
    pub on_child_spawn: Vec<HookDef>,
}
```

### Module Runtime

```rust
// newt-module/src/runtime.rs
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

pub struct ModuleRuntime {
    spec: ModuleSpec,
    registry: Arc<KitRegistry>,        // Scoped view
    permissions: PermissionEvaluator,
    resources: ResourceAccountant,
    mailbox: ModuleMailbox,
    children: RwLock<HashMap<ModuleId, Arc<ModuleRuntime>>>,
    state: RwLock<ModuleState>,
}

pub struct ModuleMailbox {
    tx: mpsc::Sender<ModuleMessage>,
    rx: RwLock<mpsc::Receiver<ModuleMessage>>,
}

#[derive(Debug, Clone)]
pub enum ModuleMessage {
    /// Request from parent or peer
    Request { id: MessageId, payload: serde_json::Value, reply_tx: oneshot::Sender<ModuleResponse> },
    /// Event broadcast
    Event { topic: String, payload: serde_json::Value },
    /// Lifecycle signal
    Shutdown { graceful: bool },
}

impl ModuleRuntime {
    pub async fn spawn(spec: ModuleSpec, parent: Option<Arc<ModuleRuntime>>) -> Result<Arc<Self>, ModuleError>;
    
    /// Execute a kit capability within this module's scope
    pub async fn call_kit(&self, call: KitCall) -> Result<KitCallResult, ModuleError>;
    
    /// Spawn child module
    pub async fn spawn_child(&self, spec: ModuleSpec) -> Result<Arc<ModuleRuntime>, ModuleError>;
    
    /// Send message to this module
    pub async fn send(&self, msg: ModuleMessage) -> Result<Option<ModuleResponse>, ModuleError>;
    
    /// Subscribe to events
    pub fn subscribe(&self, topic: &str) -> mpsc::Receiver<ModuleMessage>;
    
    /// Get resource usage snapshot
    pub async fn usage(&self) -> ResourceUsage;
    
    /// Graceful shutdown
    pub async fn shutdown(&self, graceful: bool) -> Result<(), ModuleError>;
}
```

### Scoped Kit Registry View

```rust
// newt-module/src/kit_view.rs
pub struct ScopedKitRegistry {
    base: Arc<KitRegistry>,
    scope: KitScope,
    module_id: ModuleId,
}

impl ScopedKitRegistry {
    pub fn query(&self, query: KitQuery) -> Vec<Arc<LoadedKit>> {
        let base_results = self.base.query(query);
        base_results.into_iter()
            .filter(|kit| self.scope.allows(&kit.manifest.id))
            .collect()
    }
    
    pub fn get(&self, id: &KitId) -> Option<Arc<LoadedKit>> {
        self.base.get(id).filter(|kit| self.scope.allows(&kit.manifest.id))
    }
    
    pub async fn call(&self, call: KitCall) -> Result<KitCallResult, KitError> {
        // Permission check before delegating to base
        self.permissions.check_kit_call(&call)?;
        self.base.call(call).await
    }
}
```

## Integration with Gilamonster-Agent

### Matrix Agent as Module

```rust
// gilamonster-agent/src/matrix_module.rs
pub struct MatrixAgentModule {
    runtime: Arc<ModuleRuntime>,
    agent_id: AgentId,
    role: AgentRole,
}

impl MatrixAgentModule {
    pub fn from_spec(spec: AgentSpec) -> Self {
        let module_spec = ModuleSpec {
            id: ModuleId::new(&spec.agent_id),
            name: spec.name,
            kit_scope: KitScope {
                allowed_kits: spec.required_kits,
                auto_discover_tags: vec!["role:".to_string() + &spec.role.as_str()],
                inherit_parent: true,
                denied_kits: vec![],
            },
            permissions: ModulePermissions {
                token_budget: spec.token_budget,
                api_budget: spec.api_budget,
                fs_root: Some(spec.workspace_root),
                ..Default::default()
            },
            lifecycle: LifecycleHooks {
                on_spawn: vec![HookDef::builtin("matrix-register")],
                on_shutdown: vec![HookDef::builtin("matrix-deregister")],
                ..Default::default()
            },
            ..
        };
        
        Self {
            runtime: ModuleRuntime::spawn(module_spec, None).await?,
            agent_id: spec.agent_id,
            role: spec.role,
        }
    }
}
```

### Module Hierarchy in Matrix

```
matrix-root (process)
├── coordinator-agent (module)
│   ├── planner-subagent (child module)
│   └── researcher-subagent (child module)
├── coder-agent (module)
│   ├── rust-specialist (child module)
│   └── frontend-specialist (child module)
└── reviewer-agent (module)
```

Each level:
- Inherits or overrides kit scope
- Has independent resource budgets
- Communicates via typed mailbox
- Can be migrated across processes (future)

## Implementation Phases

### Phase 1: Core Module Runtime (`newt-module`)
- ModuleSpec + ModuleRuntime
- Scoped kit registry view
- Permission evaluator integration
- Basic mailbox (in-process)

### Phase 2: Resource Accounting
- Token/API budget tracking
- Memory/CPU monitoring (best-effort)
- Budget exhaustion handling

### Phase 3: Lifecycle & Hooks
- Structured spawn/ready/shutdown
- Hook registry + execution
- Child module management

### Phase 4: Cross-Process (Mesh Integration)
- Module migration over `newt-mesh`
- Remote module proxy
- Distributed mailbox

### Phase 5: Gilamonster Integration
- `AgentSpec → ModuleSpec` mapping
- Matrix role → kit tag mapping
- Budget policies per role

## Benefits

| Concern | Before | After |
|---------|--------|-------|
| Kit isolation | Global registry | Per-module scoped view |
| Permissions | Global / ad-hoc | Structured, inheritable |
| Resources | Unlimited / process-level | Per-module budgets |
| Lifecycle | Implicit | Explicit hooks + signals |
| Multi-agent | Manual coordination | Hierarchical modules |
| Mesh migration | Not supported | Module proxy + mailbox |

## Overlaps with existing code

| Sketch concept | Existing owner |
|----------------|----------------|
| `ModuleSpec` role / kit tags | `RoleProfile`, `Loadout.kit` + `[bundles.*]` |
| Module permissions | `Caveats`, `PermissionGate`, `ExposureProfile` |
| Resource budgets | `send_budget.rs` |
| Mailbox / lifecycle | `TabSidecar`, `CrewRunner` |

## Open Questions

1. **Sync vs async hooks**: Blocking hooks simplify ordering but risk deadlock
2. **Module serialization**: For migration, need `ModuleSpec` + state snapshot
3. **Default budgets**: What are sensible defaults for token/API limits?
4. **Module identity**: UUID vs human-readable? Both?