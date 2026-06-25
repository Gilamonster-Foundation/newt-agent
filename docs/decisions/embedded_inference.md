# Embedded (in-process) inference — engine choice & feature-gating

**Status:** scaffold landed (this PR); engine increment next.
**Issue:** #639. **Related:** #559 (bulletproof summarizer), #383 (foreign providers), #548 (the summarizer-contention wedge that motivated it).

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
no C++ toolchain. Metal acceleration is an Apple-only cargo sub-feature; the
engine also runs on CPU (slower) for non-Metal hosts and CI compile-checks.

## Feature-gating (non-negotiable)

newt is amphibious (human CLI + headless swarm). The headless **wyvern** tier
must stay lean, so:

- **`embedded` cargo feature, default-off.** The `EmbeddedBackend` and (next
  increment) its candle/Metal deps live entirely behind it. The default + headless
  builds pull nothing extra; `cargo build/test/clippy/fmt` + `just cov-ci` stay
  green both ways. This is also the "add/remove from other agents we develop"
  switch — wyvern-agent (or any agent) opts in or out per build.
- **No silent downloads.** A small box (M4 / 16 GB) can't absorb surprise GGUFs;
  the backend resolves a configured model file and errors clearly when it's
  absent, naming the palette entry's HF source.

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
