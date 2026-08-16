# Feature Proposal: Animated Companion

## Overview

An animated 2D on-screen presence for the agent — a sprite-based character
(layered 2D rig driven by bone/mesh deformation rather than frame sprites;
open rig format TBD, e.g. Inochi2D or Spine-json — the Live2D Cubism SDK is
proprietary and out) that reacts to agent state (idle / listening / thinking /
speaking) and lip-syncs to TTS output. Hosted as a transparent overlay
window in the [desktop shell](desktop-shell.md) and as a panel in
[`newt-web`](tui-panel-system.md) for the browser case.

**Feature gate: `companion`.** Never in the LEAN default or wyvern/headless
build.

**Naming.** "Presence" is already taken by the WebAuthn `PresenceCaveats`
(`docs/design/human-presence-capabilities.md`), and "companion" is also the
name of wyvern sortie #1658. This doc therefore says **companion presence
state** for the on-screen character's Idle/Listening/Thinking/Speaking
state, and never bare "presence".

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
        └── rig.rs       # open-format rig driver (Inochi2D / Spine-json TBD; feature-gated)
```

### State machine, not a renderer

`newt-companion` owns a small state machine and emits render-agnostic
events; it does not itself draw anything. A `CompanionDriver` implementation
(an open-format 2D rig, or a static-sprite fallback) subscribes to
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
| Idle / Listening / Thinking | **Single source:** `OutputStream` (`newt-core/src/session.rs`) plus `newt-core` turn state. A `gilamonster-agent` matrix may *consume* this state to show many companions; it is not a source |
| Speaking + viseme timing | [`newt-speech`](speech-pipeline.md)'s `AudioChunk.visemes` |
| Manual gestures/expressions (optional) | Response tag table (widened `ThinkFilter`, `newt-core/src/reasoning.rs`; [streaming-response-categoriser.md](streaming-response-categoriser.md)) custom tag, e.g. `<expr name="happy">`, mapped through config — not a hardcoded tag list |
| Personality / voice of the character | `PersonaStore` file / `RoleProfile` ([coaching-persona.md](coaching-persona.md)) — no second persona config |

### Hosting

- **Desktop shell** — a transparent, click-through, always-on-top overlay
  window (or embedded in the tray popover); the shell's
  [capability-scoped bridge](desktop-shell.md) exposes only
  window-position/visibility calls, not raw rendering control.
- **`newt-web`** — a panel (per the [panel trait](tui-panel-system.md))
  rendering the same character via a WASM/canvas driver; state/viseme events
  arrive over the SSE reasoning/tag stream — **depends on** the response
  tag-table step landing first.
- **TUI** — explicitly out of scope; a terminal has no pixels for this.
  A minimal ASCII/emoji state indicator in `newt-tui` is a separate, much
  smaller concern and not part of this proposal.

### Milestone

| Week | Deliverable |
|------|-------------|
| 1 | `newt-companion` state machine + `CompanionDriver` trait, unit tested with a fake driver |
| 2 | Static-sprite fallback driver (four state images, no rig) — ships something usable immediately |
| 3 | Open-format rig driver (Inochi2D / Spine-json TBD) behind the `companion` feature; asset loading from a configured model path |
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
