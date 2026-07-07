# Embedded (in-process) inference — engine choice & feature-gating

**Status:** DELIVERED (2026-07-07) — the mid-loop context summarizer **defaults
to the embedded on-host CPU engine**; the `embedded` feature is **default-on**
(revised wyvern stance below). Session-reuse and off-box summarizers are
**overrides that warn**. The default model **auto-provisions on the first
interactive run** (or `newt models pull`).
**Issue:** #639, #661 (group C). **Related:** #559 (bulletproof summarizer), #383 (foreign providers), #548 (the summarizer-contention wedge that motivated it).

## Why

The summarizer is an internal, bounded, latency-sensitive call that must **never
contend with the primary model**. The #548 incident showed the failure: with the
session on a 27B DGX model, compression ran the summarizer on the *same loaded
backend*, timed out (~189s), fell back to a 3B, and barely compressed. Today's
mitigation — point `summarizer.toml` at a second always-on box — works but needs
a separate server. An **in-process** small model gives a zero-external-dependency,
single-binary path: the summarizer (and small auxiliary calls) run locally,
decoupled from the primary, with no HTTP hop.

## The seam

`InferenceBackend::endpoint()` already anticipates this: it returns `None` for
"future in-tree inference", and `None` makes the net-axis caveat check vacuous.
An embedded backend is just a new `impl InferenceBackend` whose `endpoint()` is
`None`. The summarizer selects a backend by `kind`, so `kind = "embedded"` slots
into the existing own-backend resolution.

## Engine choice: **candle + Metal**

| Engine | Nature | Metal | Verdict |
|---|---|---|---|
| **candle** (`candle-core`/`-transformers`) | pure Rust, GGUF + qwen/llama/gemma | ✅ | **chosen** — pure Rust integrates with the workspace + coverage gate; no C++ toolchain in CI |
| llama-cpp-2 | FFI to llama.cpp | ✅ | most optimized, but pulls cmake/clang + FFI build bloat |
| mistral.rs | Rust platform on candle | ✅ | higher-level but heavier dep |
| mlx-rs (Apple MLX) | Apple-native | ✅ native | fastest on M-series, but Rust bindings still early |

Pure Rust (candle) keeps the build, the lint, and the coverage gate intact with
no C++ toolchain.

## Device selection — adaptive, non-contending, smart defaults

The guiding ethos: **the code does the right thing wherever it's deployed via
smart defaults, and lets the expert (human or LLM) opt into more.** Device choice
follows that:

- **Default = CPU.** A small summarizer on a few CPU cores is fast enough, and —
  critically — it **never contends the GPU** the primary model (or another agent)
  is using. That is the whole point of #639: *decouple from the contended
  resource.*
- **Accelerators are opt-in, and the same code adapts** to whatever the box
  provides. Cargo sub-features compile them in: `embedded-metal` (Apple Silicon),
  `embedded-cuda` (NVIDIA, Linux *and* Windows). A runtime knob selects without
  recompiling: `NEWT_EMBEDDED_DEVICE = cpu (default) | metal | cuda | auto`.
  `auto` uses the first compiled accelerator that initializes, else CPU. A named
  accelerator that isn't compiled-in or fails to init **falls back to CPU** — the
  summarizer must always run, never error out over device choice.

## A first-class, general backend

`EmbeddedBackend` is a plain `impl InferenceBackend` and `kind = "embedded"` is a
first-class `BackendKind`. A `[[backends]] kind="embedded"` entry carries a local
GGUF via **`model_path`** (the in-process engine has no `endpoint`) and is
selectable *anywhere a backend is* — the crew/team pool dispatch builds it (behind
the `embedded` feature), not just the summarizer. The primitive is intentionally
small and wrappable: a future project can compose `EmbeddedBackend` under an
OpenAI/Ollama shim or something not-yet-imagined, and we'll expose it via PyO3 for
exactly that.

## Feature-gating (non-negotiable)

newt is amphibious (human CLI + headless swarm). The headless **wyvern** tier
must stay lean, so:

- **`embedded` cargo feature, DEFAULT-ON (revised 2026-07-07).** Originally
  default-off to keep wyvern lean. That stance is retired, pragmatically: the
  on-host CPU summarizer is **non-negotiable** — context compaction must never
  run on the session GPU model (it fires under peak load → overloads the GPU →
  stalls the turn, #979), so the candle engine ships in the DEFAULT build,
  **wyvern included**. `--no-default-features` (install-lean) remains the explicit
  zero-candle opt-out for anyone who truly wants it gone. Still the per-agent
  add/remove switch — the default just flipped to on.
- **One bounded auto-pull, interactive only (revised 2026-07-07).** For the
  embedded default to actually *work* out of the box, the **default** summarizer
  model auto-provisions on the first interactive `newt code` run: a single, small
  (~350 MB), named GGUF, announced with a `first pull` notice that explains it is
  offloading compaction from the GPU to the CPU, opt-out via `NEWT_NO_MODEL_PULL`,
  and gated to a TTY — a headless worker / piped / CI run **never** auto-pulls, and
  a lean (`--no-default-features`) build has no engine to pull for. A failed pull
  is **non-fatal**: resolution falls back to the warn-and-degrade (session-model)
  path. Beyond that one model nothing is silently downloaded — other palette
  entries need an explicit `newt models pull`, and the backend errors clearly
  (naming the HF source) when a configured model is absent. A
  `~/.newt/models/README.md` records what's there and why.

## The mini-model palette

`newt_inference::palette` is a curated catalog of small quantized instruct models
that fit alongside the agent on a 16 GB box (Q4_K_M, 0.5B–3B). It is available
regardless of the feature (so it can be listed/documented), while the engine that
*runs* one is feature-gated. Smallest-first; the 0.5B–1.5B entries are the safe
summarizer picks, the 3B entries the upper bound. (Remote backends —
`ollama`/`openai` — remain fully available in this space; embedded is an
*additional* option, not a replacement.)

## Phased plan

1. **Scaffold (this PR):** `BackendKind::Embedded`, the palette, the `embedded`
   feature, `EmbeddedBackend` (model resolution + the `InferenceBackend` seam,
   `endpoint() -> None`), the decision doc. `complete()` fails clearly until the
   engine lands.
2. **Engine increment (next, on-device):** the candle quantized-generation loop
   (GGUF + tokenizer → sample → decode) behind the feature, with Metal on Apple
   Silicon; wire the summarizer factory to construct it for `kind = "embedded"`;
   a BAT/UAT that compresses a large conversation with the primary idle. Validated
   on the M4 (CI has no Metal/model, so CI compile-checks the feature-on build and
   runs the feature-off lean build).

## Out of scope

Replacing the **primary** model (a 27B/256k-context DGX workload doesn't fit in
16 GB). This is the summarizer / small-auxiliary path only.
