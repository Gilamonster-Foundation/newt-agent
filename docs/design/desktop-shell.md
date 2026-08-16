# Feature Proposal: Desktop Application Shell (`newt-desktop`)

> **Status:** Draft — proposal, not normative · **Owner:** hartsock · **Last review:** 2026-08-16 · **Builds on:** `docs/decisions/newt_web_htmx.md`, `docs/decisions/newt_web_docking.md`, `docs/decisions/ocap_confinement_model.md`, `docs/decisions/agentic_object_capability_security.md`, `newt-core/src/dock_registry.rs` (`DockRegistry`), `newt-mesh/src/dock.rs` (`NewtDockService`), `newt-web/src/dock.rs`, `newt-core/src/session.rs` (`AttachRole`), `newt-web/src/csp.rs`; sibling proposals [kit-system.md](kit-system.md) (trust matrix — normative home), [module-scopes.md](module-scopes.md), [streaming-response-categoriser.md](streaming-response-categoriser.md) (`ResponseEvent`, `PresentationHint`), [tui-panel-system.md](tui-panel-system.md) (pane contract), [speech-pipeline.md](speech-pipeline.md) (`AudioFrame`, cancel epochs), [animated-companion.md](animated-companion.md) (`PresenceSnapshot`), [companion-roadmap.md](companion-roadmap.md) · **Supersedes/Superseded by:** —

Tracking issue: **#1741** (part of the companion train, EPIC #1734; index:
[companion-roadmap.md](companion-roadmap.md)).

## Overview

A native desktop shell for `newt-web` — a windowed app with a system tray,
global push-to-talk hotkey, OS-level microphone / notification consent, and a
signed auto-updater — that hosts the **same server-rendered HTMX UI** `newt-web`
already serves to a browser. It is a new *host* for an existing product, not a
second UI: the WebView is a browser pointed at a live `newt-web` server.

**Placement: a workspace-excluded crate, not a feature gate.** Two pieces:

| Piece | What it is | How it stays out of the workspace build |
|-------|------------|------------------------------------------|
| `newt-desktop` | New **workspace-excluded crate** with its own `Cargo.lock`, exactly like `newt-web` and `newt-mesh` (`docs/decisions/newt_web_htmx.md`, D1). Depends on Tauri v2 and on `newt-web` (as the sidecar binary it spawns). Versioned **independently** of the workspace, per `newt_web_htmx.md` D4 — it is a client of the newt-web loopback contract, not a workspace member. | Exclusion *is* the boundary — no workspace `--features` flag can reach it, and `just check` never sees it |
| `newt-web` sidecar mode | A Cargo feature `desktop` **on the excluded `newt-web` crate only** (off by default; `newt-web/Cargo.toml` declares no features today — this adds the first). It enables the launch-token gate tier (§Origin model), ephemeral-port announce, and the `/desktop/*` routes. Nothing about rendering changes. | Feature on an excluded crate; invisible to the workspace |

Neither piece enters the default workspace build, and neither touches the LEAN
surface or the wyvern/headless path (`docs/decisions/plain_scroller_tui.md`).

**Framework: Tauri v2**, not Electron — Rust-native, no Node runtime in a Rust
workspace, and its capabilities/ACL model gives us a *declared* WebView→native
boundary instead of an ad-hoc `postMessage` free-for-all.

## Motivation

`newt-web` (Axum + server-rendered HTMX + SSE, no JS build) attaches to a
running session through the docking seam — `DockRegistry`
(`newt-core/src/dock_registry.rs`), `NewtDockService` (`newt-mesh/src/dock.rs`),
the web side in `newt-web/src/dock.rs` (`docs/decisions/newt_web_docking.md`),
and the newt-web attach seam (planned, `newt_web_htmx.md` W6). A desktop app is
one more client of that *same* seam.

What a browser tab cannot do, and this shell must:

| Gap | Why a tab can't close it |
|-----|--------------------------|
| Persistent background presence (tray icon, close-to-tray) | A tab has no lifecycle outside the browser window |
| Global push-to-talk hotkey | Browsers only see keys while focused |
| OS microphone / notification consent owned by *our* process | `getUserMedia` consent belongs to the browser, not to newt; it cannot be scoped or revoked by us |
| Native window chrome, autostart, an always-on-top companion overlay | Browser-owned |
| Signed auto-update of the newt binary itself | Out of a web page's reach |

## Design

### Topology — three responsibilities, three trust boundaries

`newt-web` is **server-rendered**: there is no static bundle to drop into a
WebView, so the shell always runs a live `newt-web` server and everything —
document, fragments, SSE, audio — reaches it over **one loopback contract**.
Both the WebView (③) and the native host (①) are clients of that contract;
① opens no in-process channel into ②.

```
┌────────────────────────── newt-desktop (Tauri v2 core process) ───────────────────────────┐
│                                                                                            │
│  ① Privileged native host (Rust)                                                           │
│     window / tray / global hotkey / OS consent prompts / audio device capture + playback / │
│     signed updater / sidecar spawn + launch-token mint                                     │
│        ▲                                                                                   │
│        │  [B1] the bridge — Tauri IPC (`BridgeCall` ③→①, `BridgeEvent` ①→③),               │
│        │       gated by Tauri v2 capabilities/ACL                                           │
│        ▼                                                                                   │
│  ③ WebView(s): main window + optional companion overlay window                             │
│     Tauri WebView (WKWebView/WebView2/WebKitGTK) in the host process, isolated by Tauri    │
│     capabilities; document + HTMX fragments + SSE from the ONE loopback origin (§Origin)   │
└──────┬─────────────────────────────────────────────────────────┬───────────────────────────┘
       │ [B2] loopback, bearer = launch token                     │ [B2] loopback, session cookie
       │   audio-ingress route (AudioFrame, ① → ②; #1739)         │   http  document + fragments
       │   http streamed TTS audio (② → ①)                        │   sse   session output, presence,
       │   sse  presence (tray)                                    │         alignment/visemes
       ▼                                                           ▼
┌────────────────────────────────────────────────────────────────────────────────────────────┐
│  ② newt-web sidecar — the `newt-web` binary, `--features desktop`, its own process         │
│     127.0.0.1:{ephemeral} · Axum + HTMX + SSE · CSP nonce + SRI (newt-web/src/csp.rs)       │
│     speech ingress/egress endpoints (#1739) · launch-token gate                             │
│        │ [B3] dock seam — DockScope (Mirror | MirrorInject), never a second Driver          │
│        ▼                                                                                   │
│     NewtDockService / DockRegistry ──► running newt session(s) + their newt-speech sessions │
│     (speech sessions live beside the session they serve — speech-pipeline.md, fan-out note) │
└────────────────────────────────────────────────────────────────────────────────────────────┘
```

| # | Responsibility | Trust level | Owns |
|---|----------------|-------------|------|
| ① | **Privileged native host** (Tauri core, Rust) | Most privileged: full OS power of the process | Window/tray/overlay lifecycle, global hotkey, OS mic/notification consent, **audio device capture and playback**, updater, sidecar spawn, launch-token mint |
| ② | **`newt-web` sidecar server** | Trusted app code; agent-side authority is Bridle's, not the shell's | Dock attach as `DockScope::MirrorInject` — mirror + inject through the session's own input seam (`newt_web_htmx.md` **D2**, extended by `newt_web_docking.md` K1); it never holds `AttachRole::Driver` (`newt-core/src/session.rs`) — the running newt stays the sole writer. HTMX/SSE rendering, speech ingress/egress endpoints |
| ③ | **WebView** running the HTMX app | Renders server-authored HTML; may call *only* the bridge commands its Tauri capability grants | Presentation. Nothing else. |

The three trust boundaries, named:

| Boundary | Between | Mechanism | Credential |
|----------|---------|-----------|------------|
| **B1 — bridge** | ① ↔ ③ | Tauri v2 capabilities/ACL: per-window capability files, named typed commands, exact-origin scoping. The WebView is the platform engine (WKWebView / WebView2 / WebKitGTK) hosted **in the Tauri host process** and isolated by Tauri capabilities — an IPC/ACL boundary, **not a process boundary of our making**; whatever renderer-process separation exists is the platform engine's, and this design does not rely on it | Window identity + document origin |
| **B2 — loopback** | ① ↔ ② and ③ ↔ ② | HTTP + SSE (plus the audio-ingress route #1739 adds) on `127.0.0.1`, ephemeral port; every request carries the per-launch credential or is rejected | Launch token: bearer for ①, HttpOnly session cookie for ③ (§Origin model) |
| **B3 — dock seam** | ② ↔ agent | the dock seam (`DockRegistry` / `NewtDockService`; the newt-web attach route is planned, `newt_web_htmx.md` W6); `DockScope` typed authority enforced per operation; verified caller fingerprint (`newt_web_docking.md` K3) | Dock-registry approval |

**Packaging decision: ② is a spawned sidecar process** (the `newt-web` binary
in desktop mode), not a library task inside ①. Because *all* ①↔② traffic —
including audio and presence — rides the same loopback contract ③ uses, there is
exactly one client protocol and one audio transport. Embedding ② in-process
later would change nothing in that contract; it would only remove the process
boundary between the audited Tauri host and the server that renders model output.
It is therefore possible but not the default and not planned — one transport, no
second audio path (reuse discipline).

### Two authority planes — never conflated

| Plane | Governs | Mechanism | Vocabulary |
|-------|---------|-----------|------------|
| **WebView → native** | Which native calls the HTMX page may make (tray, notifications, mic *request*, overlay position, quit) | **Tauri v2 capabilities / ACL** — per-window capability files listing allowed commands and their scopes | Tauri's, and only for the shell |
| **Agent → resource** | What the *agent* may do (tools, filesystem, network, dispatch, speech providers) | **Agent Bridle** — the host-minted granted `Caveats` (the *Grant*) meet `Tool::required()` in `Gate::authorize`; `Registry` with no ambient authority | Bridle's, and only Bridle's |

The shell introduces **no third permission vocabulary**. A Tauri capability
saying "this window may invoke `request_mic_access`" says nothing about whether
an agent may read a file; a Bridle Grant says nothing about whether the WebView
may show a notification. Where the two meet — a voice turn — each side is
checked by its own plane: Tauri decides the page may *ask* for the mic; the
native host decides whether to *capture*; the STT session's authority
(`speech-pipeline.md`) is a Bridle matter on the agent side. The loopback
credential (B2) is neither: it is transport authentication ("this request came
from this launch"), not authority.

### The bridge: a typed allowlist, not general IPC

Every bridge call is a named, typed Tauri command listed in the window's
capability file with the narrowest scope Tauri supports. There is no generic
`eval` / `exec` / `invoke(any)`. Events flow the other way, and carry **host-local
state only** — everything about the *agent* (transcripts, presence, alignment)
reaches the page as SSE from ②, never over the bridge.

```rust
// sketch — illustrative, not compiled

/// ③ → ①. One Tauri command per variant; each is listed in the capability file.
pub enum BridgeCall {
    /// Run OS mic consent in ①; on success ① opens a capture session it owns and
    /// returns a scoped handle. The capture epoch is minted by ①, never chosen by
    /// the page (no replay / forged epochs).
    RequestMicAccess,                              // -> Result<CaptureSessionHandle, MicDenied>
    /// Cancel a capture the page was handed: ① bumps the capture epoch, stale frames drop.
    EndCapture { handle: CaptureSessionHandle },
    RequestNotification { title: String, body: String },
    ShowTray,
    /// Companion overlay window (animated-companion.md §Hosting): position and
    /// visibility are the ONLY overlay controls that cross the bridge. Click-through
    /// and always-on-top are static window properties ① sets when it creates the
    /// overlay (TOML config; toggled, if at all, from the tray menu — never by the page).
    SetOverlayVisible { visible: bool },
    SetOverlayPosition { x: i32, y: i32 },
    /// Hide-to-tray vs quit is host policy; the page only *requests*.
    Quit,
}

/// Opaque; grants nothing that reaches the device. `epoch` is speech-pipeline.md's
/// capture cancel epoch (never "generation" — that is Bridle's revocation counter).
pub struct CaptureSessionHandle { pub id: CaptureId, pub epoch: CancelEpoch }

/// ① → ③. Host-local UI state only — no session content ever rides here.
pub enum BridgeEvent {
    CaptureState { handle: CaptureSessionHandle, state: CaptureState, level_db: f32 },
    HotkeyState  { pressed: bool },
    /// Actual native playback position, for overlay lip-sync (§Speaker playback).
    PlaybackClock { epoch: CancelEpoch, media_time_ms: u64 },
    TrayAction(TrayActionId),
}
```

Bridge calls and events are unit-tested against a fake window/tray host
(fully-mocked unit tier); Tauri's capability files are checked into
`newt-desktop/` and reviewed as code.

### Microphone: the privileged side owns capture

OS microphone consent is **not** an object capability — it is a coarse,
per-app, user-revocable OS switch. Treating "the OS said yes" as authority to
stream audio anywhere would re-create ambient authority. So:

1. `RequestMicAccess` (or the global hotkey — same path, no page involved)
   triggers OS consent in ① — the only place that can.
2. On success ① opens a **capture session it owns** — device, format
   negotiation, resampling, capture *cancel epoch* — producing the
   `AudioFrame` stream exactly as [speech-pipeline.md](speech-pipeline.md)
   defines it (`AudioFrame { sequence, timestamp, sample_rate, channels,
   format, samples }`, media only, epoch on the `Stamped<_>` envelope). That
   doc's definition is the only one; this doc does not restate it.
3. ① posts the frames to ② over the **loopback audio-ingress route** that #1739
   adds to `newt-web` (a WebSocket or chunked-`POST` upload — TBD there; today
   `newt-web` has neither, and no browser-JS build). ① is one capture owner
   speaking that route; a browser tab with a minimal capture script is the
   other. Every frame is epoch-stamped; ② drops stale epochs.
   **Capture owner responsibilities (①):** bounded send queue toward ② — on
   overrun ① drops the *oldest* frames and lets STT see the gap via
   `TimelineMarker::Overrun` (speech-pipeline.md backpressure rule);
   **device handoff** (headset unplugged, Bluetooth switch) is ① re-opening the
   device, renegotiating the format, and resuming the `sequence`, surfaced to
   the page only as `CaptureState`.
4. The STT session (`newt.speech.stt@1 = Session<AudioFrame, TranscriptEvent>`)
   runs beside the session it serves, exactly as for a browser client — in ② for
   a session newt-web drives itself, in the agent process for a docked session
   (whose audio leg across the dock seam is #1739's open question,
   speech-pipeline.md "Relation to session fan-out"). Its Grant is
   what the browser path needs — the `net` axis for a cloud provider, `fs_read`
   over the model dir for local — and never a microphone axis: it never opens a
   device; it consumes a `Source<AudioFrame>` from the ingress. ① is the host
   process, not a governed module; what it hands over is a stream, and the
   receiving module consumes it under its own Grant.
5. The WebView receives only the **scoped `CaptureSessionHandle`** and
   `BridgeEvent::CaptureState` (level meter, "listening"): enough to show state
   and to `EndCapture`, nothing that reaches the device. Transcript
   partials/finals reach the page as ordinary SSE fragments from ②, the same
   way any other session output does. The page never calls `getUserMedia`.

**Speaker playback** is symmetric on the same contract: TTS `AudioFrame`s
stream ② → ① over the streamed-audio endpoint the browser path feeds to
`<audio>` (#1739); ① plays them through `SpeakerPlayback` and applies the
same bounded-queue / device-handoff rules on the output side. Alignment — the
`SpeechTimeline` layers `{words, phonemes, visemes, markers}` — goes ② → ③ over
SSE exactly as in the browser path, so the WebView gets captions/highlighting
*and* the visemes layer the companion overlay needs for lip-sync
([animated-companion.md](animated-companion.md) §Lip-sync). The one thing only
① knows is the real playback position, which it publishes as
`BridgeEvent::PlaybackClock`; the timeline is media-time based, so that is
sufficient to sync.

### WebView origin model

Tauri's default is to serve the frontend from an app-specific scheme origin
(`tauri://localhost` on macOS/Linux, `http://tauri.localhost` on Windows). Its
docs warn that serving from a real localhost HTTP port instead has security
implications: any local process or browser can reach a localhost listener, and
remote/HTTP origins do not get IPC unless explicitly enabled.

Decision (default; revisit only with an ADR): **the document is served from the
loopback origin — one origin for everything.**

| Aspect | Choice | Why |
|--------|--------|-----|
| Document origin | `http://127.0.0.1:{ephemeral}` — the sidecar serves the document, the HTMX fragments, SSE and the audio-ingress route. One origin. | Same-origin: `newt-web`'s routes, full-page navigations / `hx-boost`, CSP nonce + SRI (`newt-web/src/csp.rs`) and cookies all work **unchanged**. No bootstrap page, no CORS, no second CSP, no second copy of newt-web's assets |
| Rejected: app-scheme document + loopback fragments | — | Would force a bundled bootstrap page in ①; every HTMX request cross-origin (CORS + preflight on the `HX-*` headers; third-party cookies blocked); newt-web's full-page routes would navigate the WebView off the app origin; CSP binds to the *document*, so Tauri's static config would govern and newt-web's per-response nonces in swapped fragments would not match it, and `connect-src` would have to name an ephemeral port. And it removes nothing: SSE needs the loopback listener anyway |
| Tauri's warning, answered — (i) other local processes can reach the listener | They reach a 401 wall. The credential is the per-launch token, not the address; ② announces its ephemeral port to ① on stdout *after* binding, so there is no port to squat | — |
| (ii) IPC to a "remote" origin | The window's capability names the **exact** loopback origin including the launch port (`remote.urls`, registered at launch as a Tauri v2 runtime capability); ① denies WebView navigation to any other origin. Fallback if runtime registration is unavailable on a platform: a configured fixed port, bind failure = refuse to launch | — |
| (iii) rendered model output in a page with IPC | The same posture as a browser tab today: newt-web renders ammonia-sanitized HTML (`newt_web_htmx.md`), and the bridge allowlist bounds what a compromised page could reach. The origin choice does not change this risk either way | — |
| Auth: the **launch-token tier** | ① mints a random per-launch token and hands it to ② at spawn (env or stdin, never argv); ① opens the WebView at `/desktop/launch?t=<token>`; ② burns the token, sets an `HttpOnly; SameSite=Strict` session cookie and 302s to `/`. ① uses the same token as a bearer for its own loopback calls (audio, presence). Everything else is 401. Page script never sees a credential | Localhost is reachable by any local process — the token, not the address, is the credential |
| What that tier is | A **fourth D3 tier — the third *gate***. `newt_web_htmx.md` D3 already has three tiers: (1) ingress SSO, (2) self-hosted WebAuthn, (3) no gate → web chat disabled (the fail-closed default), selected by explicit config `NEWT_WEB_AUTH = ingress \| webauthn \| disabled-until-configured`. This adds a new value, `NEWT_WEB_AUTH = launch-token`, meaningful only under `newt-web --features desktop` **and** a loopback bind (any other bind with that value refuses to start) — it does not replace tier 3. It is a newt-web code change; landing it requires amending D3 in `newt_web_htmx.md` (a dependency of #1741, not something this doc can decide). In sidecar mode the WebAuthn/SSO tiers are not compiled: an IP loopback origin cannot be a WebAuthn RP anyway, and multi-agent / remote docks are the dock seam's business (`newt_web_docking.md` K3), not the browser gate's | Fail-closed, exactly one gate per bind mode |
| Script policy | newt-web's CSP nonce + SRI headers, unchanged, are the one policy; Tauri `app.security.csp` governs only app-scheme assets, of which there are none | One policy, one implementation |

### Companion overlay window

The animated companion ([animated-companion.md](animated-companion.md)) is
hosted as a **second WebView window** — transparent, click-through,
always-on-top (or a tray popover) — that loads the `newt-web` companion pane
(`View<PresenceSnapshot>`, [tui-panel-system.md](tui-panel-system.md)) from the
same loopback origin. Snapshots and the `SpeechTimeline` visemes layer arrive
over ②'s SSE like in a browser; ① contributes only the window (click-through
and always-on-top are its creation-time properties, from TOML config), the two
overlay `BridgeCall`s (`SetOverlayVisible`, `SetOverlayPosition`), and
`BridgeEvent::PlaybackClock`. The renderer,
state model and affect policy are animated-companion.md's; #1742 does not block
on this window (the newt-web pane is its first host).

### Tray + background presence

- The tray icon is a **`View<PresenceSnapshot>` host adapter in ①**: ①
  subscribes, as a loopback client with the launch token, to the same presence
  SSE stream the pane consumes. `PresenceSnapshot` (`animated-companion.md` —
  `{ actor, cognition, input, output, activity, attention, affect }`) is derived
  from the normalized `ResponseEvent` stream and turn state
  (`streaming-response-categoriser.md`) on the agent side. **No separate status
  source**; the tray never infers state from raw text.
- Model-supplied `PresentationHint`s never reach the tray: `newt-companion`'s
  affect policy is the single hint stage, and the snapshot's `affect` is already
  an approved `Option<AnimationId>`. The tray only **looks up an icon** in a
  table keyed by the tuple (`cognition`, `attention`, `affect: Option<AnimationId>`)
  — so thinking / idle / awaiting-user still show when `affect` is `None`; a
  tuple with no icon → neutral icon (animated-companion.md §Hosting says the same).
  The tray never talks over the bridge: it is a loopback SSE client in ①.
- **Global hotkey (push-to-talk)** is handled entirely in ①: hotkey down →
  OS consent (once) → new capture epoch → `AudioFrame`s → the ② audio-ingress
  route; hotkey up → finalize. The WebView is *informed*
  (`BridgeEvent::HotkeyState` / `CaptureState`), not involved.
- Window-close hides to tray by default (configurable TOML, not hardcoded);
  autostart is opt-in.

### Updater — signed or nothing

Tauri's updater plugin against a GitHub Releases (or self-hosted) manifest.
**Updates are signed**: the shell embeds the update-signing public key; the
updater rejects any manifest/artifact without a valid signature — there is no
unsigned mode. **Versioning is independent** of the workspace: `newt-desktop`
is an excluded crate and a client of the newt-web loopback contract, so it
carries its own semver and declares the newt-web contract version it speaks
(`newt_web_htmx.md` D4); the workspace release train does not bump it.

Prerequisite (**issue to be filed** under #1741): a 3-OS signing +
notarization release gate does not exist in CI today. It lands before any
public desktop build, not after.

### Plugin trust in the shell

The trust matrix's normative home is [kit-system.md](kit-system.md) (execution
/ trust matrix); this section only applies it. `newt-desktop` and anything it
loads into process ① is **trusted built-in Rust** — in-process native code
shares the process's full power, so there is no capability sandbox after
`dlopen`. The
shell therefore loads no third-party native plugins beyond the audited Tauri
plugin set it links at build time. Constrained extension (WASM component,
subprocess, remote principal) happens on the agent side under Bridle, not in
the shell. The shell loads **no kits** ([kit-system.md](kit-system.md)); the
STT/TTS kits it benefits from run on the agent side of ② as ordinary modules.

### Integration points

| Seam | Role in the shell |
|------|-------------------|
| `newt-web` sidecar / dock seam (`DockRegistry`, `NewtDockService`; attach route planned) | ② attaches with `DockScope::MirrorInject`; the HTMX UI does not know whether its host is a browser tab or a WebView |
| `newt-web --features desktop` (sidecar mode) | Launch-token gate tier (D3 amendment), ephemeral-port announce, `/desktop/launch`; rendering untouched |
| Pane contract (#1736) | The WebView is the `newt-web` pane host **verbatim**; native-only mounts (tray, overlay) are ①'s |
| `ResponseEvent` normalized stream (#1735) | Source for tray state, captions, and TTS routing — the shell consumes, never parses model text |
| `newt-speech` (#1738) + newt-web speech endpoints (#1739) | STT/TTS *sessions* on the agent side; ① owns the device ends (`MicrophoneCapture` → `AudioFrame` → audio-ingress route → STT; TTS → streamed audio → `SpeakerPlayback`), including backpressure and device handoff, and hands the page scoped handles |
| `PresenceSnapshot` / companion (#1742) | Tray projection in ①; the overlay window hosts the newt-web companion pane |
| Bridle (`Gate::authorize`, granted `Caveats`) | Untouched — the agent's authority is decided where it always was |

## Dependencies and acceptance

Ordering follows the train: Bridle authority ⇢ {Kit, Module} ⇢ {normalized event
model, presentation extension, media pipeline} ⇢ {newt-desktop, companion}.

```
Bridle authority (exists) ─┬─► Kit #1737 / Module ── not loaded by the shell; reached only through
                           │                        the newt-speech kits running on the agent side
                           │
                           └─► ResponseEvent #1735 ─┬─► pane contract #1736 (WebView = newt-web pane host) ─┐
                                                    ├─► newt-speech #1738 + newt-web speech endpoints #1739 ┤
                                                    └─► PresenceSnapshot projector #1742 ───────────────────┤
newt-web dock seam (exists) ────────────────────────────────────────────────────────────────────────────┼─► newt-desktop #1741
newt-web sidecar mode: --features desktop + D3 launch-token amendment ─────────────────────────────────┤
3-OS signing gate (prereq; issue to be filed) ─────────────────────────────────► signed updater ► public build
```

| Increment | Depends on | Acceptance |
|-----------|-----------|------------|
| Skeleton: excluded crate, sidecar spawn, one loopback origin | `newt-web --features desktop` (port announce, `/desktop/launch`) | `newt-desktop` builds outside the workspace with its own lockfile and its own version; workspace `just check` unaffected; WebView document, fragments and SSE share one origin; ② binds `127.0.0.1` on an ephemeral port and announces it after bind |
| Loopback auth | D3 amendment in `newt_web_htmx.md` | Requests without the launch credential get 401; the launch token is single-use; the session cookie is `HttpOnly`; unit-tested against a mocked sidecar |
| Bridge + Tauri ACL | skeleton | Every `BridgeCall` has a matching command in a checked-in capability file; a test asserts no command exists outside the allowlist; the capability names the exact loopback origin; navigation off-origin is denied |
| Capture owner + PTT | #1738, #1739 | `RequestMicAccess` returns a scoped `CaptureSessionHandle` with an ①-minted capture epoch; the page never calls `getUserMedia`; a stale-epoch frame is provably dropped; on a simulated overrun ① drops oldest frames and the STT side sees `TimelineMarker::Overrun`; a simulated device change renegotiates format and resumes `sequence`; frames reach the same audio-ingress route the browser path uses (contract test against a fake ingress) |
| Tray projection | #1742 projector | Tray state driven from `PresenceSnapshot` only, keyed by (`cognition`, `attention`, `affect`); a snapshot whose tuple has no icon (including an `affect` id with no icon) yields the neutral icon; `affect = None` with `cognition = Reasoning` yields the thinking icon; no `PresentationHint` type appears in `newt-desktop` (grep gate); the tray reads presence SSE, not the bridge |
| Overlay window | #1742, #1736 | Only `SetOverlayVisible` / `SetOverlayPosition` cross the bridge (click-through / always-on-top are creation-time window properties, asserted set from config in a fake-host test); `PlaybackClock` events observed by the pane in a fake-host test |
| Signed updater | 3-OS signing gate (issue to be filed) | Updater refuses an unsigned manifest in a mocked-network test |
| Release gate | all | Real window-manager / OS-consent / audio-device behavior is a release-gate manual/E2E check, not part of the per-PR unit tier |

## Cross-cutting concerns

| Concern | Approach |
|---------|----------|
| Testing | Bridge, sidecar-client and capture-owner logic unit-tested with fake hosts and a fake ingress (fully-mocked tier); OS/window/device behavior is release-gate only |
| Security | Two authority planes, no third: Tauri capabilities for WebView→native, Bridle Grant for agent→resource; loopback launch token as transport credential; CSP/SRI reused unchanged; signed updates only |
| Framework | Tauri v2, not Electron — no Node runtime; declared ACL |
| Config | Tray behavior, hotkey binding, close-to-tray, autostart, overlay defaults, update channel are TOML config (three-Cs), not constants; `desktop` is a Cargo feature on the excluded `newt-web` only; `newt-desktop` is a workspace-excluded crate, independently versioned |
| Portability of the UI | Semantic: the same HTMX app and the same panes; hosts differ only in chrome and native affordances |

## Out of scope

- Mobile shell (see `mesh-remote-control-mobile-app.md`; a separate track).
- Standing up the 3-OS signing/notarization pipeline (prerequisite; issue to be filed).
- Rendering the animated companion — [animated-companion.md](animated-companion.md)
  owns state model, affect policy and renderer; the shell contributes the overlay
  window (with its static click-through / always-on-top properties), its
  position/visibility commands and the playback clock.
- `newt-web`'s rendering model (HTMX + SSE, sanitization, CSP) and Bridle's
  authority model. The **only** newt-web change this proposal makes is the
  `desktop` feature — sidecar mode: launch-token gate tier, port announce,
  `/desktop/launch` — recorded as an amendment to D3, not as a rendering change.
  (The audio-ingress route and any capture script are #1739's, not this doc's.)
