# Feature Proposal: Animated Companion

## Overview

An animated 2D on-screen presence for the agent — a sprite-based character
(Live2D-style layered image, driven by bone/mesh deformation rather than
frame sprites) that reacts to agent state (idle / listening / thinking /
speaking) and lip-syncs to TTS output. Hosted as a transparent overlay
window in the [desktop shell](desktop-shell.md) and as a panel in
[`newt-web`](tui-panel-system.md) for the browser case.

## Motivation

A status line or spinner tells you the agent is "thinking." An animated
character makes that legible at a glance and gives voice output a visual
anchor — the same reason a phone call feels different from a voicemail.
This is a presentation-layer feature: it should consume state and audio
timing that other crates already produce, not become a second source of
truth for agent status or speech content.

## Design

### Crate: `newt-companion`

```
newt-companion/
├── Cargo.toml
└── src/
    ├── lib.rs           # CompanionState machine, driver trait
    ├── state.rs         # Idle | Listening | Thinking | Speaking { visemes }
    ├── driver.rs        # CompanionDriver trait — renderer-agnostic
    └── builtins/
        └── live2d.rs    # Live2D Cubism-style renderer binding (feature-gated)
```

### State machine, not a renderer

`newt-companion` owns a small state machine and emits render-agnostic
events; it does not itself draw anything. A `CompanionDriver` implementation
(Live2D, a simpler 2D bone-rig, or a static-sprite fallback) subscribes to
state transitions and viseme frames and renders accordingly. This keeps the
animation *technology* swappable — three-Cs discipline: which character
asset and which renderer backend are configuration, not hardcoded into the
state machine.

```rust
pub enum CompanionState {
    Idle,
    Listening,
    Thinking,
    Speaking { visemes: VisemeStream },
}

pub trait CompanionDriver: Send + Sync {
    fn on_state(&mut self, state: &CompanionState);
    fn on_viseme(&mut self, frame: VisemeFrame);
}
```

### Inputs (no new state — this is a consumer, not a producer)

| Signal | Source |
|--------|--------|
| Idle / Listening / Thinking | Agent turn lifecycle (`gilamonster-agent` matrix status, or single-agent turn state in `newt-core`) |
| Speaking + viseme timing | [`newt-speech`](speech-pipeline.md)'s `AudioChunk.visemes` |
| Manual gestures/expressions (optional) | [`newt-stream-tags`](streaming-response-categoriser.md) custom tag, e.g. `<expr name="happy">`, mapped through config — not a hardcoded tag list |

### Hosting

- **Desktop shell** — a transparent, click-through, always-on-top overlay
  window (or embedded in the tray popover); the shell's
  [capability-scoped bridge](desktop-shell.md) exposes only
  window-position/visibility calls, not raw rendering control.
- **`newt-web`** — a panel (per the [panel trait](tui-panel-system.md))
  rendering the same character via a WASM/canvas driver; state/viseme events
  arrive over the same SSE stream already used for reasoning/tag display.
- **TUI** — explicitly out of scope; a terminal has no pixels for this.
  A minimal ASCII/emoji state indicator in `newt-tui` is a separate, much
  smaller concern and not part of this proposal.

### Milestone

| Week | Deliverable |
|------|-------------|
| 1 | `newt-companion` state machine + `CompanionDriver` trait, unit tested with a fake driver |
| 2 | Static-sprite fallback driver (four state images, no rig) — ships something usable immediately |
| 3 | Live2D-style rig driver behind a feature flag; asset loading from a configured model path |
| 4 | Desktop shell overlay window hosting the driver |
| 5 | `newt-web` panel hosting the driver via canvas/WASM |

## Cross-cutting concerns

| Concern | Approach |
|---------|----------|
| Asset licensing | Character assets (rig files, sprites) are user-supplied/configured, never bundled/vendored into the workspace |
| Testing | State machine and viseme-timing logic fully unit tested with a fake `CompanionDriver`; actual rendering/animation fidelity is a manual/visual check, not a CI gate |
| Performance | Viseme frame rate is bounded and configurable; overlay window must not compete with the TUI/CLI path for CPU — this is purely additive to the LEAN surface, never required |
| Config | Which driver, which asset path, which gesture-tag mapping — all TOML config per the three-Cs convention, no hardcoded character |

## Out of scope

- 3D avatar rendering (VRM-style) — could be a future `CompanionDriver`
  implementation if there's demand, but not designed for here.
- Any change to agent turn/state semantics — this proposal is a pure
  consumer of state that other crates already own.
