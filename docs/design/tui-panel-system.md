# Feature Proposal: TUI Dashboard Panel System

> **Status (2026-08-16): Draft — partly superseded by existing code.** Premise correction:
> `newt-web` is **HTMX** and workspace-excluded (`docs/decisions/newt_web_htmx.md`,
> `newt_web_docking.md`); the only ratatui host is `newt-tui` behind the `rich-tui` feature. A
> pane contract already exists: `PanelOutcome` (`newt-tui/src/config_panel.rs`,
> `docs/decisions/harness_config_panel.md`), `TabSet` (`tabs.rs`), `newt_core::tty` widgets. This
> doc reduces to "generalise `PanelOutcome`" (roadmap A2), coordinated with #1673 / #1669.
> See the reconciliation table in [companion-roadmap.md](companion-roadmap.md).

**Scope gate.** Per `docs/decisions/plain_scroller_tui.md`, the LEAN (default) surface and the
piped/headless/wyvern path stay a plain scroller: no alternate screen, panes, or widgets there.
Everything below applies only to the feature-gated, severable, TTY-gated RichTUI (`rich-tui`)
in `newt-tui`; the wyvern tier strips the TUI entirely. Ephemeral alt-screen use follows
`docs/decisions/plan_editor_ephemeral_tui.md`.

**Naming.** `Panel` is already a `newt-scheduler` diversity-panel type; UI ADRs use **pane** /
**dock**. Reuse `PanelOutcome` as the outcome contract and prefer "pane" for new UI names.

## Overview

A **Panel System** for `newt-web` / `gilamonster-web` — a plugin architecture where UI contributions (panels, widgets, tabs, drawers) are registered by kits/modules and composed into a cohesive dashboard. Panels run in isolated contexts (WASM or iframe) and communicate via a typed message bus.

## Motivation

- `newt-tui` RichTUI (ratatui, `rich-tui` feature), `newt-web` (HTMX) and `gilamonster-web` need extensible UI
- Matrix agents should surface custom views (logs, metrics, visualizations)
- Kits/plugins should contribute UI without forking the dashboard
- Consistent model across TUI (ratatui) and Web (Leptos/Yew/Solid)

## Design

### Panel Manifest

```rust
// newt-panel/src/manifest.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelManifest {
    pub id: PanelId,
    pub version: semver::Version,
    pub metadata: PanelMetadata,
    pub mount: PanelMount,
    pub capabilities: PanelCapabilities,
    pub config_schema: Option<serde_json::Value>, // JSON Schema
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelMetadata {
    pub title: String,
    pub description: String,
    pub icon: Option<String>,      // Lucide/Phosphor icon name
    pub category: PanelCategory,   // Logs, Metrics, Debug, Custom, etc.
    pub tags: Vec<String>,
    pub author: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PanelMount {
    /// Persistent tab in main area
    Tab { 
        default_active: bool,
        order: i32,
    },
    /// Slide-over drawer (right/bottom)
    Drawer { 
        side: DrawerSide,
        default_size: u32, // pixels or percent
    },
    /// Floating modal
    Modal { 
        default_size: (u32, u32),
    },
    /// Embedded widget in existing panel
    Widget { 
        host_panel: PanelId,
        slot: String,
    },
    /// Status bar item
    StatusBar { 
        alignment: StatusAlignment,
        priority: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelCapabilities {
    /// Required kit capabilities
    pub required_kits: Vec<KitId>,
    /// Required permissions
    pub permissions: PanelPermissions,
    /// Message bus topics this panel subscribes/publishes
    pub bus_topics: BusTopics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelPermissions {
    pub read_topics: Vec<String>,
    pub write_topics: Vec<String>,
    pub kit_calls: Vec<KitId>,
}
```

### Panel Runtime (Host-Agnostic)

```rust
// newt-panel/src/runtime.rs
use std::sync::Arc;
use tokio::sync::mpsc;

pub trait PanelHost: Send + Sync {
    /// Render a panel (host-specific)
    fn render(&self, panel: &dyn PanelInstance) -> RenderOutput;
    
    /// Register a panel slot
    fn register_slot(&self, slot: PanelSlot);
    
    /// Emit event to message bus
    fn emit(&self, topic: &str, payload: serde_json::Value);
    
    /// Subscribe to message bus
    fn subscribe(&self, topic: &str) -> mpsc::Receiver<BusMessage>;
    
    /// Call a kit capability
    async fn call_kit(&self, call: KitCall) -> Result<KitCallResult, PanelError>;
}

pub trait PanelInstance: Send + Sync {
    fn manifest(&self) -> &PanelManifest;
    fn init(&mut self, ctx: PanelContext) -> Result<(), PanelError>;
    fn update(&mut self, msg: PanelMessage) -> Result<(), PanelError>;
    fn view(&self) -> PanelView;  // Host-agnostic view description
    fn on_mount(&mut self) {}
    fn on_unmount(&mut self) {}
}

pub struct PanelContext {
    pub host: Arc<dyn PanelHost>,
    pub config: serde_json::Value,
    pub kit_registry: Arc<ScopedKitRegistry>,
    pub module_id: ModuleId,
}
```

### Message Bus

```rust
// newt-panel/src/bus.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub topic: String,
    pub payload: serde_json::Value,
    pub source: PanelId,
    pub timestamp: DateTime<Utc>,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusTopics {
    pub subscribe: Vec<String>,
    pub publish: Vec<String>,
}

// Standard topics (convention)
pub mod topics {
    pub const AGENT_STATUS: &str = "matrix.agent.status";
    pub const AGENT_LOGS: &str = "matrix.agent.logs";
    pub const AGENT_METRICS: &str = "matrix.agent.metrics";
    pub const TASK_PROGRESS: &str = "matrix.task.progress";
    pub const KIT_EVENT: &str = "kit.event";
    pub const USER_ACTION: &str = "ui.user_action";
}
```

### TUI Host Implementation (`newt-tui`, `rich-tui` feature — *not* `newt-web`)

```rust
// newt-tui/src/panel_host.rs (rich-tui only)
use ratatui::prelude::*;
use ratatui::widgets::*;

pub struct TuiPanelHost {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    layout: PanelLayout,
    bus: MessageBus,
    kit_registry: Arc<KitRegistry>,
}

impl PanelHost for TuiPanelHost {
    fn render(&self, panel: &dyn PanelInstance) -> RenderOutput {
        let view = panel.view();
        match view {
            PanelView::Tabs(tabs) => RenderOutput::Tabs(tabs),
            PanelView::Split(split) => RenderOutput::Split(split),
            PanelView::Widget(widget) => RenderOutput::Widget(widget),
            PanelView::Custom(custom) => RenderOutput::Custom(custom),
        }
    }
    
    fn register_slot(&self, slot: PanelSlot) {
        self.layout.register(slot);
    }
    
    // ... bus, kit_call implementations
}

// Panel views map to ratatui widgets
#[derive(Debug, Clone)]
pub enum PanelView {
    Tabs(Vec<TabView>),
    Split(SplitView),
    Widget(WidgetView),
    Custom(Box<dyn Fn(&mut Frame, Rect) + Send + Sync>),
}

pub struct TabView {
    pub id: String,
    pub title: String,
    pub icon: Option<String>,
    pub content: PanelContent,
}

pub enum PanelContent {
    Static(Text<'static>),
    Dynamic(Box<dyn Fn() -> Text<'static> + Send + Sync>),
    Streaming(Box<dyn Fn() -> Vec<Line> + Send + Sync>),
    Interactive(Box<dyn Fn(&mut Frame, Rect, &mut AppState) + Send + Sync>),
}
```

### Web Host Implementation (`gilamonster-web`)

```rust
// gilamonster-web/src/panel_host.rs (Leptos/Solid/Yew)
use leptos::*;

#[component]
pub fn PanelHost(
    panels: Vec<PanelInstance>,
    bus: MessageBus,
    kit_registry: Arc<KitRegistry>,
) -> impl IntoView {
    let layout = use_context::<PanelLayout>();
    
    view! {
        <div class="panel-host">
            <PanelTabs panels=panels.clone() />
            <PanelDrawers panels=panels.clone() />
            <PanelModals panels=panels.clone() />
            <StatusBar panels=panels />
        </div>
    }
}

#[component]
fn PanelTabs(panels: Vec<PanelInstance>) -> impl IntoView {
    view! {
        <div class="tabs" role="tablist">
            {panels.iter().filter(|p| p.mount.is_tab()).map(|p| view! {
                <button 
                    role="tab"
                    class:active=p.is_active()
                    on:click=move |_| p.activate()
                >
                    {p.icon()}
                    {p.title()}
                </button>
            }).collect::<Vec<_>>()}
        </div>
        <div class="tab-panels">
            {panels.iter().filter(|p| p.is_active()).map(|p| p.render()).collect::<Vec<_>>()}
        </div>
    }
}
```

## Panel Examples

### Agent Log Panel (Matrix)

```rust
// gilamonster-agent/panels/log-panel/src/lib.rs
pub struct AgentLogPanel {
    agent_id: AgentId,
    log_buffer: Arc<Mutex<Vec<LogEntry>>>,
    bus_rx: mpsc::Receiver<BusMessage>,
}

impl PanelInstance for AgentLogPanel {
    fn manifest() -> PanelManifest {
        PanelManifest {
            id: "matrix.agent-logs".into(),
            mount: PanelMount::Tab { default_active: false, order: 10 },
            capabilities: PanelCapabilities {
                bus_topics: BusTopics {
                    subscribe: vec!["matrix.agent.logs.*".into()],
                    publish: vec![],
                },
                ..
            },
            ..
        }
    }
    
    fn update(&mut self, msg: PanelMessage) -> Result<(), PanelError> {
        if let PanelMessage::Bus(bus_msg) = msg {
            if bus_msg.topic.starts_with("matrix.agent.logs.") {
                let entry: LogEntry = serde_json::from_value(bus_msg.payload)?;
                self.log_buffer.lock().push(entry);
            }
        }
        Ok(())
    }
    
    fn view(&self) -> PanelView {
        let logs = self.log_buffer.lock().clone();
        PanelView::Widget(WidgetView::Streaming(Box::new(move || {
            logs.iter().map(|e| Line::from(format!("[{}] {}", e.timestamp, e.message))).collect()
        })))
    }
}
```

### Kit Metrics Panel

```rust
// newt-kit/panels/metrics-panel/src/lib.rs
pub struct KitMetricsPanel {
    kit_registry: Arc<KitRegistry>,
    refresh_interval: Duration,
}

impl PanelInstance for KitMetricsPanel {
    fn manifest() -> PanelManifest {
        PanelManifest {
            id: "newt.kit-metrics".into(),
            mount: PanelMount::Drawer { side: DrawerSide::Right, default_size: 400 },
            capabilities: PanelCapabilities {
                required_kits: vec!["newt-kit".into()],
                bus_topics: BusTopics {
                    subscribe: vec!["kit.event".into()],
                    publish: vec![],
                },
                ..
            },
            ..
        }
    }
    
    fn view(&self) -> PanelView {
        let kits = self.kit_registry.all();
        PanelView::Tabs(vec![
            TabView {
                id: "usage".into(),
                title: "Kit Usage".into(),
                content: PanelContent::Dynamic(Box::new(move || {
                    Text::from(kits.iter().map(|k| format!("{}: {} calls", k.id, k.call_count)).join("\n"))
                })),
            },
            TabView {
                id: "permissions".into(),
                title: "Permissions".into(),
                content: PanelContent::Dynamic(Box::new(move || {
                    Text::from(kits.iter().map(|k| format!("{}: {:?}", k.id, k.permissions)).join("\n"))
                })),
            },
        ])
    }
}
```

## Implementation Phases

### Phase 1: Core Panel Crate (`newt-panel`)
- Manifest + runtime traits
- Message bus (in-process)
- Panel registry + lifecycle
- Config schema validation

### Phase 2: TUI Host (`newt-tui` RichTUI)
- Ratatui panel host implementation
- Tab/drawer/modal layout engine
- Keyboard navigation + focus management
- Streaming content support

### Phase 3: Web Host (`gilamonster-web`)
- Leptos/Solid panel host
- Same manifest, different renderer
- WebSocket message bus bridge

### Phase 4: Kit Integration
- Panels as kit capabilities (`KitCapabilities.panels`)
- Auto-discovery from kit registry
- Panel permissions via module scope

### Phase 5: Matrix Panels (Gilamonster)
- Agent status grid
- Task progress timeline
- Inter-agent message flow visualization
- Resource usage dashboards

## Benefits

| Aspect | Before | After |
|--------|--------|-------|
| Dashboard extensibility | Hardcoded | Plugin panels from kits |
| TUI/Web parity | Separate codebases | Shared manifest, host-specific render |
| Agent observability | Logs only | Rich panels per agent |
| Kit introspection | CLI only | Visual panel in dashboard |
| Cross-agent views | Not possible | Message bus subscriptions |

## Open Questions

1. **WASM panels**: Compile panel logic to WASM for sandboxing?
2. **State persistence**: Panel layout/config persisted where?
3. **Hot reload**: Panel code reload without dashboard restart?
4. **Accessibility**: TUI accessibility (ratatui-a11y) parity with web?