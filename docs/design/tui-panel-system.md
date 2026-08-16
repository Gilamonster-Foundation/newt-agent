# Feature Proposal: Pane System (RichTUI, newt-web, desktop)

> **Status:** Draft — proposal, not normative · **Owner:** hartsock · **Last review:** 2026-08-16 · **Builds on:** `docs/decisions/plain_scroller_tui.md`, `lean_rich_tui_morphologies.md`, `harness_config_panel.md` (`PanelOutcome`, `newt-tui/src/config_panel.rs` — a transient `Viewport::Inline` overlay), `plan_editor_ephemeral_tui.md` (the alt-screen carve-out), `session_tabs.md` (`TabSet`/`TabSidecar`, `newt-tui/src/tabs.rs`), `newt-tui/src/rich_input.rs` (pinned `Viewport::Inline` region), `newt_web_htmx.md`, `newt_web_docking.md` (`DockRegistry`, `newt-core/src/dock_registry.rs`; `NewtDockService`, `newt-mesh/src/dock.rs`; the web side, `newt-web/src/dock.rs`), `ocap_confinement_model.md`, `agentic_object_capability_security.md`; `newt_core::session::{OutputStream, OutputChunk, AttachRole}`; `newt_core::tty` widget suite; [streaming-response-categoriser.md](streaming-response-categoriser.md) (`ResponseEvent`); [kit-system.md](kit-system.md) (`InterfaceId`, trust matrix); [companion-roadmap.md](companion-roadmap.md) · **Supersedes/Superseded by:** —

**Tracking:** #1736 (A2 — pane contract) under the companion train EPIC #1734. Coordinated with
the in-flight #1673 (slash commands → RichTUI panes) and the #1669 tab train.

## Scope gate and naming

- **Scope.** Per `plain_scroller_tui.md`, the LEAN (default) surface and the piped / headless /
  wyvern path stay a plain scroller: no alternate screen, panes, or widgets there. Everything
  below applies only to (1) the feature-gated, severable, TTY-gated **RichTUI** (`rich-tui`
  feature of `newt-tui`), (2) **newt-web** (Axum + server-rendered HTMX + SSE, an *excluded*
  workspace member with its own lockfile), and (3) the **newt-desktop** WebView, which hosts the
  same HTMX app ([desktop-shell.md](desktop-shell.md)). The wyvern tier strips the TUI entirely.
- **RichTUI has two mount classes, and only one of them touches the alternate screen.**
  Everything persistent in RichTUI today is a bounded row region of a ratatui
  `Viewport::Inline` (`rich_input.rs`: "no alternate screen"; the `TabSet` bottom bar,
  `session_tabs.md`: "no alternate screen, ever"), and the two existing transient overlays —
  the `/psyche` config panel (`config_panel.rs` line 3, `Viewport::Inline(height)` at line 798)
  and the `/backend` chooser (`backend_panel.rs`, #1667, `rich-tui`-gated, following the #1665
  house panel grammar) — are *transient* `Viewport::Inline` overlays. The **only** alt-screen
  carve-outs are the splash and the `/plan` editor
  (`plan_editor_ephemeral_tui.md`). This document keeps those two classes distinct throughout
  and does not cite the `/plan` policy for inline overlays.
- **Naming.** `PanelOutcome` already exists **twice**: `newt-scheduler/src/panel.rs` (the
  diversity / verification panel — `PanelConfig`, `PanelOutcome { status, accepted, votes }`) and
  `newt-tui/src/config_panel.rs` (the psyche/config ephemeral panel — `PanelOutcome { Cancelled |
  Applied {..} | Saved { name } | SavedAndApplied {..} }`); the `/backend` panel has its own exit
  contract, `backend_panel::PanelClose { apply, remove_after_apply, changes }`. New UI names
  therefore use **pane** (and **dock**, per `newt_web_docking.md`); this document says "pane"
  throughout, and "Panel" appears only when quoting existing code. When the two overlays' exit
  contracts are generalised, the new type is **`PaneOutcome` in `newt-tui`** — never a third
  `PanelOutcome`. The pane interface
  id is **`newt.ui.pane@1` = `View<PaneModel>`** ([kit-system.md](kit-system.md) interface
  table; the `<namespace>.<name>@<major>` id format is kit-system.md's).

## Overview

A **pane** is a host-projected view contributed by a kit or a built-in feature — an agent log,
a kit-metrics readout, the psyche/config editor, a text-only companion strip — that can be
mounted as a tab, drawer, modal, embedded widget, status-bar item, or (desktop only) tray item. This
proposal defines **one pane contract** that three hosts project:

| Host | Surface | Existing seam to widen |
|------|---------|------------------------|
| `newt-tui` RichTUI | ratatui, `rich-tui` feature only. Two mount classes: **(a)** inline-viewport rows and overlays (`Viewport::Inline` — `TabSet` bottom row, `rich_input` region, the `/psyche` and `/backend` overlays); **(b)** the ephemeral alt-screen carve-out (`/plan`-style editors) | `PanelOutcome` (`config_panel.rs`), `PanelClose` (`backend_panel.rs`), `TabSet`/`TabSidecar` (`tabs.rs`), `newt_core::tty` widgets, #1673 slash-command panes |
| `newt-web` | Axum + HTMX partials + SSE, no JS build; the pane runs **server-side** | `DockRegistry` (`newt-core/src/dock_registry.rs`), `NewtDockService` (`newt-mesh/src/dock.rs`), `newt-web/src/dock.rs`, and the newt-web attach seam (planned — `newt_web_htmx.md` W6) |
| `newt-desktop` | Tauri WebView running the newt-web HTMX app over loopback | Reuses the newt-web host verbatim for in-window mounts; adds a **thin native adapter in the privileged host** for `tray` / `notification` mounts. The tray adapter (`View<PresenceSnapshot>`, [desktop-shell.md](desktop-shell.md)) reads the presence SSE stream directly as a loopback client — *not* over the Tauri bridge; only `notification` goes over the bridge (`RequestNotification`) |

Three things this contract deliberately is not: a closure over a ratatui `Frame` presented as a
portable view (that is a **RichTUI** renderer); a topic bus with its own read/write lists (a
**second permission vocabulary** beside Agent Bridle); or a Leptos/JS front end (the web host
is server-rendered HTMX).

## Where a pane sits in the stack

The companion train uses one vocabulary across its documents:

| Layer | Question it answers | For panes |
|-------|--------------------|-----------|
| **Kit** | what code / interface is available | a kit's manifest *exports* the pane interface `newt.ui.pane@1` (`View<PaneModel>`) and names its implementation location and execution class |
| **Module** | who is running, with what Grant | the module hosting the pane's kit instance holds the host-minted granted `Caveats` (the *Grant*) that every callable the pane holds is authorized against |
| **Bridle** | what authority exists | `Gate::authorize(tool, granted)` computes `granted.meet(tool.required())` per `Tool`, at dispatch; there is no pane-level permission engine |
| **Interface** | what can be composed | `newt.ui.pane@1` is the `View<State>` base shape from [kit-system.md](kit-system.md) with `State = PaneModel`; what a pane *consumes* is likewise named by ids whose shape is `Source<…>` |
| **Event** | what happened | panes consume `ResponseEvent` ([streaming-response-categoriser.md](streaming-response-categoriser.md), which also owns the one `ResponseEvent` → `OutputStream`/`OutputChunk` mapping) and today's `OutputChunk`/`OutputStream`; they emit typed pane events |
| **View** | how a host projects it | the *only* host-specific layer: RichTUI widgets, HTMX partials over SSE, WebView, native tray |

Everything above the View row is host-neutral by construction because it never mentions a
renderer.

## The pane contract

A pane declares three things and receives a small set of handles. It never sees a terminal, a
`Frame`, an HTML fragment, or a bus.

### What a pane declares

```rust
// sketch — illustrative, not compiled
/// Manifest fragment exported by a kit (see kit-system.md). Describes
/// interfaces and *required* authority; it never grants anything.
pub struct PaneManifest {
    pub id: PaneId,                       // stable, e.g. "newt.agent-log"
    pub interface: InterfaceId,           // "newt.ui.pane@1" = View<PaneModel>
    pub metadata: PaneMetadata,           // title, description, icon hint, category, tags
    pub mounts: Vec<MountPref>,           // MountIds it is willing to occupy (host picks)
    pub consumes: Vec<InterfaceId>,       // ids whose shape is Source<…>, e.g.
                                          //   "newt.session.response@1" (Source<ResponseEvent> — enveloped, streaming-response-categoriser.md)
                                          //   "newt.events.turn@1"      (Source<TurnState>, sketched below)
    pub provides: Vec<InterfaceId>,       // e.g. "newt.events.pane@1"   (Source<PaneEvent>)
    pub actions: Vec<ActionDecl>,         // command_id + request/response shape
    pub required: Caveats,                // declarative — the axes the pane's *callables*
                                          // need (fs_read/fs_write/exec/net, max_calls,
                                          // valid_for_generation); Scope::none() on every
                                          // axis for a subscription-only pane
    pub config_schema: Option<serde_json::Value>,
}

/// Mounts are host-declared identifiers discovered like InterfaceIds (three Cs: a new host
/// mount is data, not a variant). Well-known ids: "tab", "drawer", "modal", "widget",
/// "statusbar", "tray", "notification". Unknown ids are ignored by hosts that lack them.
pub struct MountPref { pub mount: MountId, pub hints: MountHints }

/// HOST HINTS, not neutral semantics: a host may honour, clamp or ignore any of them.
/// `SizeHint` in rows/cols is meaningful to RichTUI, advisory to HTMX (mapped to a CSS class),
/// meaningless to a tray. Nothing routes or authorizes on a hint.
pub struct MountHints { pub order: Option<u16>, pub size: Option<SizeHint>, pub side: Option<Side>, pub priority: Option<u8> }
```

`mounts` is a *preference list*, not a command: a RichTUI "modal" is a transient inline
overlay (today's `/psyche` panel), HTMX renders a "drawer" as a swapped partial, and only the
desktop host offers `tray` — the other two ignore that id.

**`required` is exactly the Bridle lattice, nothing more.** agent-mesh-protocol 0.6.3
`Caveats` has the axes `fs_read`/`fs_write`/`exec`/`net` (`Scope<String>`), `max_calls`
(`CountBound`) and `valid_for_generation`. There is **no** session-attach or registry-read axis,
and this document does not invent one: subscription needs are declared by `consumes` and
satisfied by host-issued handles (below); adding a `Caveats` axis would be a cross-crate
agent-mesh-protocol change and is out of scope here.

### What a pane receives (capability handles, not topics)

```rust
// sketch — illustrative, not compiled
pub struct PaneContext<S: SteeringSlot = NoSteering> {
    /// Scoped subscriptions the host issued (e.g. Subscription<ResponseEvent> for one session,
    /// attached as AttachRole::Observer — read-only is enforced by SessionRegistry).
    pub subscriptions: Vec<Subscription>,
    /// Scoped, typed publishers: the pane can emit PaneEvent to whoever the host wired.
    pub publishers: Vec<Publisher>,
    /// Callable handles: each wraps a Bridle `Tool` plus the module's granted `Caveats`.
    /// The handle limits WHICH tools the pane can name; every invocation is still
    /// `Gate::authorize` (or `authorize_with_discharge`) at call time.
    pub callables: Vec<Callable>,
    /// Steering is a *type-level* slot: `NoSteering` (a ZST with no methods) unless the
    /// manifest declares "newt.input.steering@1", in which case the host constructs
    /// `PaneContext<Handle<dyn SteeringInbox>>`. A pane compiled against the default
    /// context has no `.steer(..)` to call — nothing to unwrap, nothing to check.
    pub steering: S,
    pub config: serde_json::Value,
    pub principal: PrincipalId,                // the hosting module's AgentKey id (module-scopes.md); for accounting
}

/// What a pane's `update` receives — the union of everything the host wired.
pub enum PaneInput {
    Event(ResponseEvent),                      // from a Subscription<ResponseEvent>
    Turn(TurnState),                           // from Source<TurnState>
    Presence(PresenceSnapshot),                // from Source<PresenceSnapshot> (companion strip)
    Action { command_id: CommandId, args: serde_json::Value },   // a declared action, host-dispatched
    Config(serde_json::Value),                 // config changed
    Resize(SizeHint),                          // host hint only
}

/// Minimal generalisation of the two existing overlay exit contracts — the config panel's
/// `PanelOutcome` (newt-tui/src/config_panel.rs) and the backend panel's `PanelClose`
/// (newt-tui/src/backend_panel.rs) — what an *ephemeral* pane hands back when it closes.
/// Lives in `newt-tui`; deliberately not a third `PanelOutcome`. `Applied` / `Saved` /
/// `apply` / `remove_after_apply` / `changes` details are pane-specific payloads, not variants.
pub enum PaneOutcome {
    Cancelled,
    Committed { detail: serde_json::Value },   // config panel: Applied / Saved / SavedAndApplied fold here;
                                               // backend panel: PanelClose { apply, remove_after_apply, changes } folds here
}

/// The agent-turn state a pane can subscribe to (`newt.events.turn@1 = Source<TurnState>`).
/// Sketched HERE because this doc maps it from today's `SessionState`; owned by `newt-core`
/// (`newt_core::session`), derived from `SessionState` + `AttachRole` — illustrative variants:
pub enum TurnState {
    Idle,
    Running { turn: u64 },
    AwaitingPermission { turn: u64 },          // PermissionGate prompt outstanding
    Cancelling { turn: u64 },
}
// speech-pipeline.md (`VoiceTurnEvent` is the *human's* spoken turn, a different thing) and
// animated-companion.md (`attention` / `cognition` inputs) cite this definition.
```

The rules that make this a single authority plane:

1. **The host issues handles; the pane holds them.** A `Subscription<ResponseEvent>` for
   session S is a capability: possession *is* the authorization. There is no
   `read_topics`/`write_topics` list to check against and no `PermissionEvaluator`. Attach
   role is decided at attach time (`AttachRole::Observer` for read-only panes) and enforced by
   `SessionRegistry` — `newt-core/src/session.rs`: "enforced here, not by the observer's
   caveats".
2. **Every callable is a Bridle `Tool`, authorized per dispatch.** `Gate::authorize(tool,
   granted)` is per-`Tool` (`Tool::required()`) and runs at call time; `max_calls: CountBound`
   and `valid_for_generation` cannot be `meet()`'d once into a handle. What the host does at
   mount is narrower: it hands the pane a `Callable` only for tools whose `Tool::required()`
   is `leq` the module's Grant, so a pane cannot *name* a tool outside its Grant — and each
   invocation is still gated.
3. **Attenuation only ever narrows.** A pane hosted by a child module holds
   `parent.grant.meet(requested).meet(host_clamp)` (cf. #739). A pane can hand a *narrower*
   handle to a widget it embeds; it cannot widen one.
4. **No ambient bus.** Cross-pane communication is a `Publisher`/`Subscription` pair the host
   wired deliberately. "Standard topics" become **typed event interfaces** (`Source<AgentStatus>`,
   `Source<TaskProgress>`, …) exported by kits under stable ids, discoverable through the kit
   registry, not string constants.
5. **Budgets are not permissions.** Refresh cadence, retained-line caps, and render budget
   are `ResourceBudget` on the module (Resources axis), separate from `Caveats` (Authority
   axis).

### What a pane consumes

Panes route on the **typed normalized event stream** rather than scraping text:

| Input | Today | Target |
|-------|-------|--------|
| Model output | `OutputChunk { turn, stream: OutputStream, seq, data, last }` fanned out by `SessionRegistry` to attachments | `ResponseEvent { Text, Reasoning, ToolCall, ToolResult, Artifact, PresentationHint, Done }` per [streaming-response-categoriser.md](streaming-response-categoriser.md), whose projection table is the one mapping between the two |
| Turn / session state | `SessionState`, `AttachRole` | derived into `TurnState` (sketched above, owned by `newt-core`) and exposed as `Source<TurnState>` under `newt.events.turn@1` |
| Transcript | `newt-core/src/agentic/transcript.rs` types | `Source<TranscriptEntry>` |
| Human input | `InputSurface` (`newt-tui/src/chat.rs`), `SteeringInbox` | pane gets a `SteeringInbox` **handle** (typed slot above) only if its manifest declares it and the host issues it |
| Presence | — | `Source<PresenceSnapshot>` from the projector ([animated-companion.md](animated-companion.md)) |

An "agent thought" pane subscribes to `Reasoning` events; a diff pane to `Artifact`/`Diff`;
an activity pane to `ToolCall`/`ToolResult`. The **RichTUI companion strip is a pane** —
`newt.ui.pane@1` whose `PaneModel` the host adapter derives from `PresenceSnapshot` — a
text-only glyph/status line, **not the animated companion** (the rig/sprite renderer lives only
in the `newt-web` / desktop hosts, animated-companion.md). It is fed by
`Source<PresenceSnapshot>` and never subscribes to raw `PresentationHint`; hints are untrusted
model output that pass through the projector's affect policy to an approved `AnimationId`
first. None of these panes re-parse `<think>` tags — that is the compatibility adapter's job
(`ThinkFilter` lineage, `newt-core/src/reasoning.rs`).

**Hints on the wire.** `PresentationHint` (shape defined once in
streaming-response-categoriser.md: `{ kind, value, attrs, span, source: PrincipalId }`) is
*dropped* by today's `OutputChunk` projection. A pane hosted in `newt-web` or reached through
the dock seam therefore cannot see hints — and neither can a `newt-web`-hosted companion
projector — until `OutputStream` is widened (A1-b in that doc). **Requirement:** A1-b carries
`PresentationHint` through `OutputChunk`; until it lands, hint-consuming panes are in-process
(RichTUI) only.

## Portability: two options, one recommendation

There are two honest answers to "host-neutral".

### Option A — semantic portability via host adapters (recommended first step)

The pane declares **state, events, and actions**; each host owns presentation through an
adapter.

```rust
// sketch — illustrative, not compiled
pub trait Pane: Send + Sync {
    type Model: Serialize + Clone;              // the state a host projects
    fn manifest(&self) -> &PaneManifest;
    fn init(&mut self, ctx: PaneContext) -> Result<(), PaneError>;
    fn update(&mut self, ev: PaneInput) -> Result<(), PaneError>;   // subscription events, actions
    fn model(&self) -> Self::Model;             // pure; no rendering
    fn outcome(&self) -> Option<PaneOutcome>;   // generalisation of PanelOutcome (below)
}

/// One adapter per (pane, host). RichTUI: draws Model with newt_core::tty / ratatui into an
/// inline-viewport region (or, for editors, the alt-screen carve-out). newt-web: renders
/// Model to an HTMX partial and pushes it over SSE. desktop: the WebView reuses the newt-web
/// adapter; a thin native adapter in the privileged host covers "tray" (fed by presence SSE over
/// loopback) and "notification" (over the Tauri bridge) mounts.
pub trait PaneAdapter<M> { /* host-specific: fn render(&self, model: &M, ...) */ }
```

- **Pros:** matches what already exists (the config panel's `PanelOutcome` + `PanelState`,
  `newt-tui/src/config_panel.rs`, are already "pure model, host draws"); each host renders
  idiomatically (ratatui widgets vs. HTMX partials); no layout engine to invent; ships
  incrementally per pane.
- **Cons:** N panes × M hosts adapters. Partly mitigated because the desktop host is the web
  host and a pane with no adapter for a host simply is not offered there. **But "a kit ships
  its own adapters" is only true for trusted kits** — a RichTUI adapter is a closure over a
  ratatui `Frame` running in the `newt-tui` process, i.e. in-process native code:

| Kit execution class ([kit-system.md](kit-system.md) trust matrix) | Trust | Who may supply the host adapter |
|---|---|---|
| Built-in Rust | trusted | the kit ships pane + adapters (today's `/psyche` panel) |
| Native dylib (`dlopen`) | trusted-only — shares the process's full power, no capability sandbox after load | kit-shipped adapter allowed, because the kit is already fully trusted |
| WASM component | constrained | **model over the wire only** (plugins-protocol); the host supplies a generic adapter |
| Subprocess | constrained by the Bridle enforcement floor | model over the wire only; host generic adapter |
| Remote principal | constrained principal + delegated grant | model over the wire only; host generic adapter |

  A "host generic adapter" that renders an arbitrary kit's `Model` is Option B by another
  name. So the concrete forcing function toward Option B is **the first constrained-execution
  kit that exports `newt.ui.pane@1`**, not merely the "three similar panes" heuristic below.

### Option B — declarative presentation IR (ambitious)

The pane emits a small **presentation tree** that every host renders generically:

```rust
// sketch — illustrative, not compiled
pub enum Node {
    Column(Vec<Node>), Row(Vec<Node>),
    Text(StyledText), Table { header: Vec<String>, rows: Vec<Vec<StyledText>> },
    Gauge { label: String, ratio: f32 },
    Button { label: String, command_id: CommandId },        // an action the pane declared
    Stream { subscription_id: SubscriptionId, tail: usize }, // host tails a subscription
}
```

- **Pros:** one renderer per host, zero per-pane adapters; a remote pilot or a log can render
  it too; matches how HTMX already thinks (server describes, client swaps); it is the only
  shape a WASM / subprocess / remote kit can use at all (table above).
- **Cons:** invents a layout/IR now, before any second host is live; every host must implement
  every node (RichTUI `Table` in `newt_core::tty`, HTMX `Gauge`, …); expressive panes (diff
  viewer, timeline) either outgrow the IR or push it toward a widget toolkit — exactly the
  sprawl the reuse discipline warns about.

### Recommendation

**Adopt Option A now; keep Option B as the growth path.** Concretely: generalise the config
panel's `PanelOutcome` and the backend panel's `PanelClose` into `PaneOutcome` (sketch above), keep
`PanelState`-style pure models,
and add adapters as panes are ported to RichTUI (#1673) and newt-web. Extract Option B's `Node` from existing adapters (data first,
then IR — the three Cs) when *either* trigger fires: three or more panes are pure
`Table`/`Text`/`Gauge` compositions across both hosts, **or** a constrained-execution kit
(WASM / subprocess / remote) needs to export a pane. Until then, only trusted (built-in / dylib)
kits ship panes.

## Host mapping

| `MountId` | RichTUI (`newt-tui`, `rich-tui`) | newt-web (HTMX + SSE) | newt-desktop (WebView + native) |
|-----------|----------------------------------|-----------------------|---------------------------------|
| `tab` | persistent `TabSet` entry with a `TabSidecar` — bottom row of the inline viewport (`session_tabs.md`; no alt-screen) | tab strip partial; SSE-driven swap | as newt-web |
| `drawer` | bounded transient inline-viewport region of N rows beside the scroller (config-panel style); no side split exists today — hosts may decline the id | `hx-swap` into a drawer region | as newt-web |
| `modal` | transient `Viewport::Inline` overlay returning `PaneOutcome` (today's `/psyche` config panel, `config_panel.rs`, and `/backend` chooser, `backend_panel.rs`); an *editor* that needs the whole screen uses the alt-screen carve-out under `plan_editor_ephemeral_tui.md` (today: `/plan`) | dialog partial | as newt-web, or native dialog via the Tauri bridge |
| `widget` | slot inside a host pane's model | nested partial | as newt-web |
| `statusbar` | persistent bottom line of the inline viewport via `newt_core::tty` / `rich_input` (never on the LEAN scroller) | footer partial | footer |
| `tray` | not offered | not offered | native tray item (`View<PresenceSnapshot>` adapter in the privileged host, fed by the presence SSE stream over loopback — never the bridge; [desktop-shell.md](desktop-shell.md)) |
| `notification` | not offered | not offered | native notification via the Tauri bridge (`RequestNotification`) |

RichTUI-specific notes:

- **`PaneOutcome` generalises both existing overlay exit contracts.** The config panel's
  `PanelOutcome` variants — `Cancelled`, `Applied {..}`, `Saved {..}`, `SavedAndApplied {..}` —
  encode the editor's commit semantics and fold into `Committed { detail }`; the backend panel's
  `PanelClose { apply: Option<BackendSelection>, remove_after_apply, changes: Vec<String> }`
  (`PanelClose::cancelled()` = `Default`) folds the same way (`Cancelled` when `apply` is `None`
  and `changes` is empty; otherwise `Committed { detail }`). The generalisation keeps the
  invariant both panels already fix (`harness_config_panel.md`; `backend_panel.rs` "exit
  contract"): the caller reports from freshly-resolved runtime state after committing, never from
  the pane's working copy. (`newt-scheduler`'s `PanelOutcome` is untouched — it is not a UI type.)
- **The real mount rule.** *Persistent* mounts (`tab`, `statusbar`) are bounded rows of the
  `Viewport::Inline` region — never the alternate screen, never on LEAN or wyvern. *Modal /
  editor* mounts are transient inline overlays (the `/psyche` panel today) or, exceptionally,
  the alt-screen carve-out (`/plan` today). CLAUDE.md permits RichTUI to "host panes / a live
  dock overview" — bounded inline rows are exactly that; **always-on full-screen dashboards**
  stay out of newt (gilamonster-agent / monitor repos).
- **#1673 is the on-ramp.** Slash commands that open panes are the first consumers of the
  contract: the slash-command grammar #1673 defines (`/command [args]` → open / focus / close a
  pane) **extends the existing #1665 house panel grammar** (`←`/`→` dial, `Enter` apply, `Esc`
  cancel — what `/psyche` and `/backend` already speak) rather than starting fresh, and it is *the
  RichTUI adapter's* input grammar — it produces `PaneInput::Action` for the pane and mount/focus
  requests for the RichTUI host; it is not part of the pane contract itself.
  Each command's pane should be written as a pure model + RichTUI adapter so the newt-web adapter
  is a partial away.
- **ratatui placement, precisely.** `ratatui` is already a non-optional dependency of `newt-tui`
  (code and pilot modes); what the `rich-tui` feature gates is the inline-viewport surface
  (`Viewport::Inline`, `tui-textarea`) — and pane mounts live only there. So the accurate rule
  is "pane rendering only behind `rich-tui`", not "ratatui only behind `rich-tui`".

newt-web-specific notes:

- The dock seam — `DockRegistry` (`newt-core/src/dock_registry.rs`), `NewtDockService`
  (`newt-mesh/src/dock.rs`), `newt-web/src/dock.rs`, plus the newt-web attach seam (planned,
  `newt_web_htmx.md` W6) — already models "a session reachable through an authorized caller"; a
  pane subscription is a scoped view of exactly that. Panes do not bypass `authorize_caller`.
- **Two planes, not one.** In the HTMX host the pane runs **server-side**, so its
  `Subscription` / `Publisher` / `Callable` handles are ordinary in-process handles — the
  browser never holds a capability handle. What travels over HTTP is (a) SSE-pushed
  re-rendered partials of the pane `Model` and (b) `hx-post` requests from the browser bound
  to a declared `command_id`, which the host turns into `PaneInput::Action` before the pane
  (still server-side) acts on it. No JS build.

## Examples (semantic form)

### Agent log pane

```rust
// sketch — illustrative, not compiled
pub struct AgentLogPane { tail: VecDeque<LogLine>, cap: usize }

impl Pane for AgentLogPane {
    type Model = Vec<LogLine>;
    fn init(&mut self, ctx: PaneContext) -> Result<(), PaneError> {
        // ctx.subscriptions holds exactly one Subscription<ResponseEvent> for one session —
        // issued by the host, attached as AttachRole::Observer. Nothing to look up, nothing
        // to check. ctx.steering is NoSteering: there is no method to call.
        Ok(())
    }
    fn update(&mut self, ev: PaneInput) -> Result<(), PaneError> {
        if let PaneInput::Event(ResponseEvent::ToolCall(d) | ResponseEvent::ToolResult(d)) = ev {
            self.tail.push_back(LogLine::from(d));
            if self.tail.len() > self.cap { self.tail.pop_front(); }   // ResourceBudget, not Caveats
        }
        Ok(())
    }
    fn model(&self) -> Self::Model { self.tail.iter().cloned().collect() }
    fn outcome(&self) -> Option<PaneOutcome> { None }                 // read-only pane
}
```

RichTUI adapter: `newt_core::tty` scroll region in an inline overlay. newt-web adapter: `<ul>`
partial appended over SSE. Manifest: `consumes = ["newt.session.response@1"]` (satisfied by a
host-issued observer subscription), `required = Caveats` with `Scope::none()` on every axis —
no callables, so nothing for Bridle to gate.

### Kit metrics pane

Manifest `consumes = ["newt.kit.registry_events@1"]` (`Source<KitRegistryEvent>`, a
host-issued subscription over `newt-core/src/kit.rs` `RegistryEntry` state — no "registry-read
caveat" exists or is needed), `provides = ["newt.events.pane@1"]`; model is a table of
`(kit id, tier, calls, provenance CID)` — provenance shown, not "permissions", because kits
declare *required* authority and the module holds the Grant ([kit-system.md](kit-system.md),
[module-scopes.md](module-scopes.md)). If a "reload kit" action is added later, that is
a `Callable` over a Bridle `Tool` whose `Tool::required()` names the `exec`/`fs_read` scopes
it needs, gated per invocation.

## Dependencies and acceptance criteria

No calendar. Ordering follows the companion-roadmap dependency graph: Bridle authority ⇢
Kit/Module ⇢ **normalized event model + presentation extension (this doc)** ⇢ desktop,
companion.

| Item | Depends on | Acceptance |
|------|-----------|------------|
| `PaneOutcome` generalisation of the config panel's `PanelOutcome` and the backend panel's `PanelClose` | nothing new (RichTUI, `harness_config_panel.md`, `backend_panel.rs` #1665/#1667) | existing `/psyche` and `/backend` panels pass unchanged through `PaneOutcome`, still as `Viewport::Inline` overlays (their existing exit-contract tests keep passing against the fold); unit tests are pure-model, no terminal; `newt-scheduler`'s `PanelOutcome` is not renamed or touched |
| `PaneContext` handles | Bridle `Gate::authorize`, module Grant ([module-scopes.md](module-scopes.md)) | a pane whose manifest declares no steering is compiled against `PaneContext<NoSteering>` — there is no handle and no method to call (structural absence; no runtime permission check is consulted); a `Callable` for a tool outside the Grant does not appear in `callables`, and each call that does exist still goes through `Gate::authorize` |
| `MountId` discovery | kit registry ids ([kit-system.md](kit-system.md)) | a host that lacks a mount id ignores it; adding `tray` for desktop touched no RichTUI or newt-web code |
| Panes consume `ResponseEvent` | [streaming-response-categoriser.md](streaming-response-categoriser.md) adapter (#1735) | agent-thought pane shows only `Reasoning`; a `<think>` chunk split across arbitrary boundaries never leaks into a text pane (streaming fixtures, #1506); the companion strip pane consumes `PresenceSnapshot`, never a raw `PresentationHint` |
| Hints reach web/dock panes | A1-b `OutputStream` widening (#1735) | a `PresentationHint` emitted in-process arrives, with its `source: PrincipalId`, at a `newt-web`-hosted pane over the dock seam (contract test against a fake attachment) |
| RichTUI adapters for #1673 panes | above | every slash-command pane has a pure model + adapter; persistent mounts are inline-viewport rows only; LEAN scroller and wyvern paths untouched (`plain_scroller_tui.md` test guard) |
| newt-web adapters | `DockRegistry` authorization, the planned attach seam, SSE | same pane model renders as an HTMX partial; browser holds no handles — only `command_id`-bound `hx-post`; no JS build introduced |
| Desktop reuse | [desktop-shell.md](desktop-shell.md) | WebView renders the newt-web adapter output unchanged; `tray` / `notification` are additive native mounts |
| Trust matrix for adapters | [kit-system.md](kit-system.md) execution classes | only built-in / dylib (trusted) kits ship adapters; a constrained kit exporting `newt.ui.pane@1` is refused until Option B exists |
| Option B IR (deferred) | ≥3 panes on ≥2 hosts that are pure `Table`/`Text`/`Gauge`, **or** the first constrained-execution kit exporting a pane | `Node` is extracted from existing adapters, not designed ahead |

## Open questions

1. **State persistence.** Where does per-user pane layout live — user-scoped config under
   `~/.newt` (the `newt-identity` `UserKey`'s home; keyed by `PrincipalId` where a per-principal
   layout is wanted — *not* `agent-identity.toml`, which is the git / GitHub-App commit identity), or
   the loadout/bundle (`Loadout.kit`, `docs/design/loadout-composition.md`)?
2. **Generic adapter floor.** When Option B lands for constrained kits, does the host generic
   adapter also become the *default* for trusted kits (one renderer per host), or do trusted
   kits keep shipping bespoke adapters for expressive panes (diff, timeline)?
3. **Accessibility.** RichTUI has no a11y story; the HTMX host inherits the browser's. Does
   Option A make RichTUI the accessibility floor, and is that acceptable?
4. **Remote pilot.** Should the mesh remote-control surface
   (`docs/design/mesh-remote-control-mobile-app.md`) be a fourth host with its own adapters,
   or the first real consumer of Option B?

## Change log

- 2026-08-16: dropped the `newt-panel` crate / `PanelHost` trait / ambient topic bus /
  `PanelPermissions` / `PanelView::Custom(Box<dyn Fn(&mut Frame, Rect)>)` shape in favour of the
  pane contract above (semantic model + per-host adapters, capability handles, Bridle-only
  authority); the interface id settled on `newt.ui.pane@1`.
