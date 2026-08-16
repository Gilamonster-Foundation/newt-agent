# Airi → Newt/Gilamonster Integration Roadmap

**Objective**: Extract the highest-leverage patterns from Airi (TypeScript/Node, Electron + Vue + Capacitor) and map them into Newt (Rust, TUI-first) and Gilamonster (multi-agent matrix) to unblock **newt-web** and **gilamonster-web**.

---

## 1. Kit System → `newt-kit` crate (Newt workspace member)

### Airi pattern
- **`src/shared/kit.ts`** — Unified registry for Skills, Tools, MCP servers, Plugins
- Each "kit" has: `manifest`, `capabilities`, `remoteCallable` policy, `install()`, `uninstall()`, `enable()`, `disable()`
- Kits are discoverable from: local fs, npm, git, HTTP
- Capability caveats = attenuated permissions per-kit

### Newt mapping
| Airi concept | Newt crate / type |
|--------------|-------------------|
| `KitRegistry` | `newt_kit::Registry` (single global, `Arc<RwLock<Registry>>`) |
| `KitManifest` | `KitManifest { id, version, kind: KitKind, capabilities, caveats, remote_policy }` |
| `KitKind` | `enum KitKind { Skill, Tool, McpServer, Plugin, Panel }` |
| `Capability` | Reuse `newt_skills::SkillCaveats` + extend with `ToolPerm`, `McpPerm`, `PluginPerm` |
| `remoteCallable` | `RemotePolicy { allow: Vec<PeerId>, deny: Vec<PeerId>, max_depth: u8 }` |

### Crate skeleton (new workspace member)
```
newt-kit/
├── Cargo.toml
└── src/
    ├── lib.rs           # Registry, Kit, KitKind, Manifest, Caveats
    ├── discovery.rs     # fs::read_dir, git clone, HTTP fetch
    ├── install.rs       # Copy/Link/Compile (wasm for panels)
    ├── remote.rs        # RemotePolicy, mesh attestation verification
    └── builtins.rs      # Ship core kits: reasoning, git, fs, shell, web-search
```

### Integration points
- **newt-skills** → becomes a `KitKind::Skill` source (no API change, just re-export)
- **plugins-protocol** → `KitKind::Plugin` wraps `PluginClient` with caveats
- **newt-mcp-client** → `KitKind::McpServer` registers tools via `KitRegistry`
- **newt-web** → `KitKind::Panel` = WASM module loaded in iframe (see §3)

### Milestone
| Week | Deliverable |
|------|-------------|
| 1 | `newt-kit` crate compiles, `Registry::discover_local()` works |
| 2 | `install()`/`enable()`/`disable()` with caveat enforcement |
| 3 | RemotePolicy + mesh attestation verification (reuse `newt-mesh::plugin_envelope`) |
| 4 | Built-in kits registry; `newt-core` depends on `newt-kit` |

---

## 2. Module Scopes → Gilamonster "Agent = Module" mapping

### Airi pattern
- **`src/main/module.ts`** — `ModuleScope` = isolated permission boundary + kit allowlist + lifecycle
- Each module: `permissions`, `allowedKits`, `onDispose` hooks
- Modules communicate only via typed events (Eventa IPC)
- Parent process spawns module workers; crashes are contained

### Gilamonster mapping
| Airi concept | Gilamonster equivalent |
|--------------|------------------------|
| `ModuleScope` | `AgentInstance` (one per matrix cell) |
| `allowedKits` | `AgentManifest.kits: Vec<KitId>` — declarative in `agent.toml` |
| `permissions` | `AgentManifest.caveats` (attenuated from parent `UserKey`) |
| `Eventa IPC` | `agent-mesh` session streams (QUIC, attested) |
| `onDispose` | `Drop` impl + `AgentInstance::shutdown()` |

### Agent manifest (new file: `agent.toml` per agent)
```toml
[agent]
id = "coder-01"
name = "Newt Coder"
version = "0.1.0"

[kits]
# Explicit allowlist — no ambient authority
reasoning = "builtin"
git = "builtin"
web-search = "npm:@gilamonster/web-search@^1.0"
custom-panel = "git:https://github.com/me/panel-kit#v2"

[caveats]
# Attenuated from parent UserKey
fs = { read = ["/workspace/**"], write = ["/workspace/out/**"] }
net = { allow = ["api.github.com", "*.anthropic.com"] }
tools = { allow = ["read", "write", "edit", "bash"] }
```

### Matrix integration
- **Scheduler** (`newt-scheduler`) spawns `AgentInstance` per manifest
- **Mesh gateway** routes `session_stream` per-agent (not per-process)
- **KitRegistry** is per-process but `allowedKits` filters visibility
- **Crash isolation**: each agent = separate tokio task + panic hook → `AgentInstance::restart()`

### Milestone
| Week | Deliverable |
|------|-------------|
| 1 | `AgentManifest` parsing + validation in `newt-core` |
| 2 | `AgentInstance::spawn(manifest)` with kit filter + caveat enforcement |
| 3 | Mesh session per-agent (reuse `newt-mesh::DockClient`) |
| 4 | Supervisor: restart, health-check, graceful drain |

---

## 3. Panel/Plugin System → newt-web TUI Dashboard

### Airi pattern
- **`src/renderer/panels/`** — Vue components registered as "panels"
- **Gamelet Kit** — Iframe-backed sandbox: `postMessage` API, CSP, permission prompt
- Panels contribute: tabs, sidebars, modal overlays, status bar items
- Hot-reload in dev; WASM bundle in prod

### newt-web mapping (HTMX + WASM, not Vue)
| Airi concept | newt-web implementation |
|--------------|-------------------------|
| Panel component | **WASM module** exporting `Panel` trait (via `wit`/`wasm-component-model`) |
| Iframe sandbox | `<iframe sandbox="allow-scripts" src="wasm:/panel-name">` |
| `postMessage` | **`PanelHost`** JS shim → `worker.postMessage` → WASM `canonical_abi` |
| Tab/sidebar/overlay | Server-rendered HTMX fragments + WASM mount points |
| Hot-reload | `newt-web` watches `panels/` dir, recompiles WASM, pushes `hx-swap-oob` |

### Panel trait (shared via `wit` — `newt-panel-wit` crate)
```wit
package newt:panel@0.1.0;

interface panel {
    // Called once when panel mounts
    init(config: config) -> result<(), error>;

    // Render a fragment (HTMX-compatible HTML string)
    render(ctx: render-context) -> string;

    // Handle HTMX events from the fragment
    on-event(event: panel-event) -> result<action, error>;

    // Cleanup
    dispose() -> ();
}

record config { kit-id: string, agent-id: string, permissions: list<string> }
record render-context { session-id: string, turn: u64, viewport: viewport }
record viewport { width: u32, height: u32 }
variant panel-event { click(id: string), input(id: string, value: string), ... }
variant action { swap(html: string), notify(title: string, body: string), ... }
```

### Crate layout
```
newt-panel-wit/          # WIT definitions only (no Rust code)
newt-panel-host/         # JS shim + iframe loader (npm pkg, consumed by newt-web)
newt-web/
  ├── panels/            # Built-in panels (WASM source)
  │   ├── sessions/      # Session list + transcript
  │   ├── agents/        # Matrix view (Gilamonster)
  │   ├── skills/        # Kit browser + installer
  │   └── mesh/          # Peer presence + dock
  └── src/
      ├── panel_registry.rs  # Loads .wasm, instantiates, manages lifecycle
      └── routes.rs          # /panel/:name → streams fragment
```

### Milestone
| Week | Deliverable |
|------|-------------|
| 1 | `newt-panel-wit` published; `newt-panel-host` npm pkg works in isolation |
| 2 | `panel_registry` loads WASM, calls `init`/`render`/`dispose` |
| 3 | Built-in panels: sessions, agents, skills (HTMX + minimal WASM) |
| 4 | Gilamonster matrix panel (agent grid, live logs, kit status) |
| 5 | Hot-reload dev loop (`cargo watch` + `wasm-pack` + HTMX swap) |

---

## 4. Response Categoriser → `newt-stream-tags` crate

### Airi pattern
- **`src/shared/streaming/response-categoriser.ts`** — Incremental XML tag parser on token streams
- Extracts `<think>`, `<tool>`, `<file>`, custom tags *as they arrive*
- Filters TTS: speaks only non-tagged content, queues tagged for later
- Emits `onTagOpen(tag)`, `onTagClose(tag)`, `onTagContent(tag, chunk)`

### Newt mapping
- Current: `reasoning::split_reasoning` — hardcoded tags, whole-string only
- New: `newt_stream_tags::TagStream` — `Stream<Item = TagEvent>` wrapper around any `AsyncRead`/`AsyncBufRead`

```rust
pub enum TagEvent {
    Open { name: String, attrs: HashMap<String, String> },
    Content { name: String, chunk: Bytes },
    Close { name: String },
    Text(Bytes),  // outside any tag
}

pub struct TagStream<R> { inner: R, buffer: Vec<u8>, state: State }
impl<R: AsyncBufRead> Stream for TagStream<R> { type Item = Result<TagEvent>; ... }
```

### Use cases
- **newt-tui**: Live reasoning panel (collapsible `<think>`), tool-call preview
- **newt-web**: Server-sent events → `TagStream` → HTMX fragments per tag
- **TTS gateway**: Filter `Text` events only, buffer tagged for "read later"
- **Audit log**: Structured `<tool>`, `<decision>` tags → append-only log

### Milestone
| Week | Deliverable |
|------|-------------|
| 1 | `TagStream` core + tests (malformed, nested, split across chunks) |
| 2 | `newt-tui` integration: reasoning panel + tool preview |
| 3 | `newt-web` SSE endpoint: `/api/stream/tags?session=...` |
| 4 | TTS filter example binary |

---

## 5. Sequenced Milestones → newt-web / gilamonster-web

```
┌─────────────────────────────────────────────────────────────────────┐
│ PHASE 0: Foundation (weeks 1-2)                                     │
├─────────────────────────────────────────────────────────────────────┤
│ • newt-kit crate + Registry + local discovery                       │
│ • AgentManifest + caveats in newt-core                              │
│ • newt-panel-wit published                                          │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│ PHASE 1: Agent = Module (weeks 3-4)                                 │
├─────────────────────────────────────────────────────────────────────┤
│ • AgentInstance::spawn with kit filter + caveat enforcement         │
│ • Mesh session per-agent (DockClient)                               │
│ • Supervisor: restart, health, drain                                │
│ • newt-stream-tags crate                                            │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│ PHASE 2: newt-web MVP (weeks 5-7)                                   │
├─────────────────────────────────────────────────────────────────────┤
│ • panel_registry + WASM load + init/render/dispose                  │
│ • Built-in panels: sessions, agents, skills                         │
│ • HTMX fragments + SSE tag stream                                   │
│ • WebAuthn + session persistence (already in newt-web)              │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│ PHASE 3: gilamonster-web (weeks 8-10)                               │
├─────────────────────────────────────────────────────────────────────┤
│ • Matrix panel: live agent grid, kit status, mesh topology          │
│ • Cross-agent session correlation (shared ConversationStore)        │
│ • Remote kit install via mesh (peer advertises KitManifest)         │
│ • Panel hot-reload dev loop                                         │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│ PHASE 4: Ecosystem (week 11+)                                       │
├─────────────────────────────────────────────────────────────────────┤
│ • Kit registry protocol (mesh-gossip KitManifest)                   │
│ • WASM panel sandbox (CSP, capability prompts)                      │
│ • Mobile: newt-mobile consumes same panel WASM via Capacitor        │
│ • Airi ↔ Newt panel interop (shared WIT)                            │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Cross-cutting concerns

| Concern | Decision |
|---------|----------|
| **WASM target** | `wasm32-wasip1` (component model) — `wasm-pack` + `cargo component` |
| **Kit distribution** | Git + npm + local fs; mesh-gossip for peer discovery (Phase 4) |
| **Caveat format** | Extend `SkillCaveats` → `KitCaveats` (serde tag = "kind") |
| **Versioning** | Semver per-kit; `Registry` resolves via `semver::VersionReq` |
| **Security** | Panels = WASM + iframe CSP; no `eval`, no host bindings except `PanelHost` |
| **Testing** | `newt-kit` = unit + property; panels = `wasm-bindgen-test` + Playwright (newt-web) |

---

## Repo / crate creation checklist

- [ ] `newt-kit` — workspace member, deps: `newt-skills`, `newt-mcp-client`, `plugins-protocol`, `agent-mesh-protocol`
- [ ] `newt-panel-wit` — separate crate (WIT only), published to GitHub Packages
- [ ] `newt-panel-host` — npm package (TypeScript), `peerDependencies: newt-panel-wit`
- [ ] `newt-stream-tags` — workspace member, zero deps except `tokio`, `bytes`
- [ ] Update `newt-core` Cargo.toml → depend on `newt-kit`, `newt-stream-tags`
- [ ] Update `newt-web` → add `panel_registry`, WASM panel build pipeline
- [ ] Add `agent.toml` schema + example to `newt-core/examples/`
- [ ] CI: `cargo test --workspace`, `wasm-pack test --headless --firefox`, Playwright

---

## Why this unblocks newt-web / gilamonster-web

| Blocker | Resolved by |
|---------|-------------|
| No plugin/panel extension point | Kit System + Panel trait (WASM) |
| No multi-agent dashboard | Matrix panel + per-agent mesh sessions |
| Hardcoded reasoning tags | `newt-stream-tags` + SSE |
| Skills/Tools/MCP/Plugins fragmented | Unified `KitRegistry` |
| No agent isolation / crash containment | `AgentInstance` + `ModuleScope` pattern |
| No hot-reload for web UI | WASM recompile + HTMX `hx-swap-oob` |

---

## Next immediate actions

1. **Create `newt-kit` crate skeleton** (copy `newt-skills` structure, add `KitKind`, `Registry`)
2. **Publish `newt-panel-wit`** (minimal WIT, `cargo publish --dry-run`)
3. **Add `AgentManifest` to `newt-core`** (reuse `SkillCaveats` as base)
4. **Spike `TagStream`** in `newt-stream-tags` (100 loc, property tests)

Want me to start on any of these?