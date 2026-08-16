# Feature Proposal: Speech Pipeline (STT / TTS)

## Overview

A provider-agnostic **speech pipeline** crate that turns a token stream from
inference into scheduled, interruptible audio playback (TTS), and turns a
microphone/audio stream into transcribed text (STT). Both directions plug
into the existing kit seam (`newt-core/src/kit.rs`, `Loadout.kit` /
`[bundles.*]` — see [kit-system.md](kit-system.md)) as kit components /
bundle entries, so any local or cloud backend can be swapped without
touching pipeline logic. There is no `newt-kit` crate and no
`KitKind::Speech`; speech providers ride the kit/bundle mechanism that
already exists.

**Feature gate: `speech`** (on `newt-tui` / `newt-cli`). Never compiled
into the wyvern/headless path or the LEAN default build — the pipeline is
severable and additive, per `docs/decisions/plain_scroller_tui.md`.

### Seams this design consumes (all existing)

| Need | Existing seam |
|------|---------------|
| Text to speak | `OutputStream` / `OutputChunk` / `OutputSink` (`newt-core/src/session.rs`), filtered to chunks the response tag table marks `Speech`-eligible |
| What is speakable vs. not | The response tag table (a widening of `ThinkFilter`, `newt-core/src/reasoning.rs`; see [streaming-response-categoriser.md](streaming-response-categoriser.md)) — no `newt-stream-tags` crate |
| Publishing STT text as input | `SteeringInbox` (`newt-core/src/agentic/steering.rs`) mid-turn; `InputSurface` (`newt-tui/src/chat.rs`) at the prompt |
| Provider registration/config | `newt-core/src/kit.rs` component + `Loadout.kit` / `[bundles.*]` entry |
| Permission | `Caveats` (`newt-core/src/caveats.rs`) — new caveat kinds `audio.in` (mic) and `audio.out` (speaker) |

## Motivation

Voice input/output is the highest-friction feature to bolt on later if the
core isn't designed for it: TTS needs to consume partial LLM output as it
streams (not wait for the full response), needs to be interruptible
mid-sentence, and needs a segmentation strategy so it doesn't read code
blocks or tool-call XML aloud. STT needs to handle streaming audio, silence
detection, and partial/final transcript events. Bolting these on after the
fact means retrofitting the inference loop; designing for them now means one
clean seam.

Design goals:
- A **provider registry** classifying backends by task (`speech-to-text` /
  `automatic-speech-recognition` vs `text-to-speech`) with both local
  (on-device model) and cloud (API) implementations behind the same
  interface, so a user can pick "local whisper" or "cloud STT" without the
  app caring.
- A **speech pipeline** that segments an incoming text-token stream into
  speakable chunks, resolves priority/interruption between concurrent
  "intents" (e.g. a new reply should interrupt an in-progress read-aloud),
  and schedules playback with start/end/interrupt/reject lifecycle events.
- A **transcript buffer** that accumulates streaming STT partials into a
  stable final transcript, with silence-based utterance boundaries.

## Design

### Crate: `newt-speech`

```
newt-speech/
├── Cargo.toml
└── src/
    ├── lib.rs           # SpeechPipeline, TranscriptionPipeline
    ├── provider.rs      # SpeechProvider, TranscriptionProvider traits
    ├── segmenter.rs      # TextSegment stream: split token stream into speakable chunks
    ├── priority.rs      # Intent priority + interrupt/queue/replace resolution
    ├── playback.rs      # PlaybackItem scheduling, start/end/interrupt/reject events
    ├── transcript.rs    # Streaming STT partial → stable transcript buffer
    └── builtins/        # Local (whisper.cpp / piper) + passthrough providers
```

### Provider traits (kit-registered)

```rust
#[async_trait]
pub trait SpeechProvider: Send + Sync {
    /// Synthesize one text segment to audio. Cancelable via the AbortSignal-equivalent.
    async fn synthesize(&self, req: TtsRequest, cancel: CancellationToken) -> Result<AudioChunk>;
}

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    /// Feed an audio chunk, get zero or more transcript events back (partial or final).
    async fn transcribe_chunk(&self, audio: AudioChunk) -> Result<Vec<TranscriptEvent>>;
}

pub enum TranscriptEvent {
    Partial { text: String, utterance_id: String },
    Final { text: String, utterance_id: String },
    SilenceDetected { utterance_id: String },
}
```

`AudioChunk` carries an optional `visemes: Vec<VisemeFrame>` timing track
when the provider can produce one (most local TTS engines can; cloud
providers vary) — consumers that don't care about mouth-shape timing just
ignore the field. This is the only seam [animated-companion.md](animated-companion.md)
needs from this crate.

Both traits register as kit components in `newt-core/src/kit.rs`, selected
by a `Loadout.kit` / `[bundles.*]` entry, discovered the same way as any
other kit component (builtin, local fs, or a bundle `.toml` pointing at a
local model path / cloud endpoint + credentials).

### Tag table vs. segmenter — who decides what

- The **response tag table** decides *what is speakable*: it classifies
  each `OutputChunk` (prose, `<think>`, tool-call, code block, custom tags)
  and only chunks tagged `Speech`-eligible are handed to this crate. That
  is model/prompt-domain knowledge, so it lives in the tag table's data,
  not here.
- The **segmenter** decides *how to chunk* what it is given: sentence /
  clause boundaries, pause hints, and maximum utterance length. It never
  re-classifies content; if code is being read aloud, the fix is in the tag
  table, not the segmenter.

### Intent-based TTS scheduling

Each intent declares a `behavior` of queue / interrupt / replace — needed so, e.g., a tool-status narration can
queue behind the current reply, while a user interruption or a new turn can
cut off playback immediately rather than waiting for the sentence to finish.

```rust
pub struct SpeakIntent {
    pub intent_id: IntentId,
    pub turn_id: Option<TurnId>,
    pub priority: u8,
    pub behavior: InterruptBehavior, // Queue | Interrupt | Replace
}
```

### Integration points

- **`OutputStream`** (`newt-core/src/session.rs`) — the pipeline is one
  more `OutputSink`; the response tag table (widened `ThinkFilter`,
  [streaming-response-categoriser.md](streaming-response-categoriser.md))
  runs first so `<think>`/tool-call content is filtered out of what gets
  spoken (only `Speech`-eligible chunks reach the segmenter). Depends on the
  tag-table step landing.
- **`newt-tui`** (feature `speech`) — mic capture → `TranscriptionPipeline`
  → final transcript is published to `SteeringInbox` while a turn is running,
  or into `InputSurface` at the prompt; partials shown as a live
  "listening…" line.
- **`newt-web`** — browser mic via WebAudio/WebSocket → same
  `TranscriptionPipeline`; playback via `<audio>` element fed by a streamed
  response endpoint.
- **`newt-core/src/kit.rs`** — provider discovery/config via a bundle
  entry; the bundle declares `Caveats` kinds `audio.in` / `audio.out` so
  speech capability is an explicit, attenuable permission like fs/net.

### Milestone

| Week | Deliverable |
|------|-------------|
| 1 | `newt-speech` crate: traits + segmenter + priority resolver, unit tested with fake providers |
| 2 | Builtin local providers (whisper.cpp binding for STT, piper/coqui for TTS) |
| 3 | `newt-tui` integration: push-to-talk mic input, spoken reply playback |
| 4 | `newt-web` integration: browser mic + streamed audio response endpoint |
| 5 | Cloud provider adapters (behind kit config, no code change to pipeline) |

## Cross-cutting concerns

| Concern | Approach |
|---------|----------|
| Real-audio testing | Unit tier fully mocked (fake `SpeechProvider`/`TranscriptionProvider`); real device/mic tests live in the expensive weekly/release tier only, per the repo's testing strategy |
| Permission model | Mic/speaker access is `Caveats` `audio.in` / `audio.out`, not ambient — a bundle without them cannot register a speech provider |
| Backpressure | Bounded channel between segmenter and TTS scheduler; configurable cap on concurrent synthesis tasks |
| Silence/VAD | Pluggable voice-activity-detection strategy behind `TranscriptionProvider`, not hardcoded — three-Cs: VAD thresholds are config, not code |

## Out of scope

- On-device wake-word detection (separate proposal if needed).
- Rendering an animated presence driven by this audio — see
  [animated-companion.md](animated-companion.md), which
  consumes this crate's `AudioChunk`/viseme timing as its input.
