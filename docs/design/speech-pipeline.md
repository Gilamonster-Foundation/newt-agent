# Feature Proposal: Speech Pipeline (STT / TTS)

## Overview

A provider-agnostic **speech pipeline** crate that turns a token stream from
inference into scheduled, interruptible audio playback (TTS), and turns a
microphone/audio stream into transcribed text (STT). Both directions plug
into the [Kit System](kit-system.md) as `KitKind::Speech` /
`KitKind::Transcription` providers, so any local or cloud backend can be
swapped without touching pipeline logic.

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

Both traits register into `newt_kit::Registry` as `KitKind::Speech` /
`KitKind::Transcription`, discovered the same way as any other kit (builtin,
local fs, or a `provider.toml` pointing at a local model path / cloud
endpoint + credentials).

### Intent-based TTS scheduling

Mirrors the reference pipeline's `behavior: 'queue' | 'interrupt' |
'replace'` per-intent model — needed so, e.g., a tool-status narration can
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

- **`newt-inference`** — feeds the token stream into the segmenter; segments
  route through [`newt-stream-tags`](streaming-response-categoriser.md)
  first so `<think>`/tool-call content is filtered out of what gets spoken
  (only `TagEvent::Text` reaches the speech pipeline).
- **`newt-tui`** — mic capture → `TranscriptionPipeline` → inserts final
  transcript as user input; partials shown as a live "listening…" line.
- **`newt-web`** — browser mic via WebAudio/WebSocket → same
  `TranscriptionPipeline`; playback via `<audio>` element fed by a streamed
  response endpoint.
- **`newt-kit`** — provider discovery/config; a kit manifest can declare
  `caveats.audio = { mic: bool, speaker: bool }` so speech capability is an
  explicit, attenuable permission like fs/net.

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
| Permission model | Mic/speaker access is a kit caveat, not ambient — an agent module without `audio` caveats cannot register a speech provider |
| Backpressure | Bounded channel between segmenter and TTS scheduler; `ttsMaxConcurrent`-equivalent cap on concurrent synthesis tasks |
| Silence/VAD | Pluggable voice-activity-detection strategy behind `TranscriptionProvider`, not hardcoded — three-Cs: VAD thresholds are config, not code |

## Out of scope

- On-device wake-word detection (separate proposal if needed).
- Rendering an animated presence driven by this audio — see
  [animated-companion.md](animated-companion.md), which
  consumes this crate's `AudioChunk`/viseme timing as its input.
