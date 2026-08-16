# Feature Proposal: Desktop Application Shell

## Overview

A native desktop shell for `newt-web` — a windowed app with a system tray,
mic/speaker permission prompts, and native OS integration (autostart, global
hotkey, notifications) — hosting the same `newt-web` HTMX UI that runs in the
browser, rather than a second, divergent UI implementation.

**Feature gate: `desktop`.** `newt-desktop` is a workspace-excluded crate
with its own lockfile, exactly like `newt-web`
(`docs/decisions/newt_web_htmx.md`) — it never enters the default workspace
build, the LEAN surface, or the wyvern/headless path.

## Motivation

`newt-web` (HTMX, not ratatui; `docs/decisions/newt_web_htmx.md`) attaches
to a running session through the docking seam — `NewtDockService` /
`DockRegistry` (`newt-core/src/dock_registry.rs`,
`docs/decisions/newt_web_docking.md`) and its `/attach` endpoint. A desktop
app is another client of that *same* seam — not a new UI to build. (Any
future panel layout per [tui-panel-system.md](tui-panel-system.md) is a
`newt-web` concern the shell inherits for free; it is not a dependency of
this proposal.) The main gap a browser tab can't
close: persistent background presence (tray icon, "always listening" mic
for the speech pipeline), OS-level permission prompts for mic/screen
capture, native window chrome, and auto-update.

Design goal — a three-process split: **main** (window/tray lifecycle, OS
permission prompts, auto-update), **renderer** (the actual UI, reused from
the web app almost unmodified), **bridge** (a narrow, capability-scoped
IPC surface between the two, not a general postMessage/IPC free-for-all).
That process boundary is the load-bearing design decision here, independent
of which UI framework sits in the renderer.

## Design

### Process split

```
newt-desktop/
├── Cargo.toml
└── src/
    ├── main.rs          # Window/tray lifecycle, OS integration, updater
    ├── bridge.rs         # Narrow capability-scoped IPC surface (see below)
    └── permissions.rs    # Native mic/screen/notification permission prompts
```

Built on `tauri` (Rust-native, avoids pulling a Node/Electron runtime into a
Rust workspace) rather than Electron — the renderer is the existing
`newt-web` HTMX/WASM UI loaded as a local/embedded site, not a rewrite.

### Capability-scoped bridge, not general IPC

An unbounded IPC surface between main and renderer invites the same
"no ambient authority" problem the
[Module Scopes proposal](module-scopes.md) solves for agents. The
desktop bridge should expose exactly the calls the renderer needs and
nothing else, each gated by the same caveat model:

```rust
pub enum BridgeCall {
    RequestMicAccess,
    RequestNotification { title: String, body: String },
    ShowTray,
    Quit,
    // No generic "eval" or "exec" — every call is a named, typed capability
}
```

### Tray + background presence

- Tray icon reflects agent state (idle / thinking / speaking), read from
  the docked session's `OutputStream` / turn state via the dock seam — no
  separate status source.
- Global hotkey (push-to-talk) forwards directly into
  [`newt-speech`](speech-pipeline.md)'s `TranscriptionPipeline`; the
  desktop shell owns *permission prompting* for the mic, not audio capture
  logic itself.
- Window-close hides to tray rather than quitting, matching the "always
  available" assistant pattern — configurable, defaulting to on.

### Update channel

`electron-updater`-equivalent: `tauri`'s built-in updater against a
GitHub Releases or self-hosted update manifest. Reuses the workspace's
existing semver scheme (see CLAUDE.md Versioning) — no separate versioning
scheme for the desktop build.

### Integration points

- **`newt-web` / `NewtDockService`** — the renderer loads the existing
  HTMX UI and attaches via `/attach`; the web UI doesn't know or care
  whether its host is a browser tab or a desktop webview.
- **`newt-speech`** — mic/speaker access, gated through the bridge's
  permission prompts rather than an unprompted browser `getUserMedia`.
- **`gilamonster-web`** (optional, later) — multi-agent matrix status
  could additionally feed the tray, but the single-session dock seam is the
  source of truth here.

### Milestone

| Week | Deliverable |
|------|-------------|
| 1 | `newt-desktop` skeleton: window + tray, loads `newt-web` dashboard locally |
| 2 | Bridge: typed `BridgeCall` surface, mic/notification permission prompts |
| 3 | Global hotkey → `newt-speech` push-to-talk; tray reflects agent state |
| 4 | Auto-update wiring against release manifest |
| 5 | Packaging: signed builds for macOS/Windows/Linux — **prerequisite issue**: no 3-OS signing release gate exists in CI today; file and land that first |

## Cross-cutting concerns

| Concern | Approach |
|---------|----------|
| Testing | Bridge calls unit-tested with a fake window/tray host (per the repo's fully-mocked unit tier); real window-manager behavior is a release-gate-only manual/E2E check |
| Security | Bridge is an explicit allowlist, no generic IPC/eval — same "no ambient authority" discipline as kit caveats |
| Framework choice | `tauri`, not Electron — keeps the shell in the Rust workspace instead of introducing a Node runtime dependency |
| Config | Tray behavior, hotkey binding, and update channel are TOML config, not hardcoded, per the three-Cs convention |

## Out of scope

- Mobile app shell (no `newt-mobile` crate exists; if one is proposed it
  would be a separate track).
- Standing up the 3-OS code-signing release pipeline (tracked as its own
  prerequisite issue).
- Rendering the animated companion itself — see
  [animated-companion.md](animated-companion.md); the
  desktop shell hosts it as one more panel/window layer, but the renderer
  and state model belong to that proposal.
