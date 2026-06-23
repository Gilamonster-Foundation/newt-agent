# `newt dgx vllm` — stand up and configure a vLLM server on the DGX

Status: **proposed** · Phase 14 follow-on to `newt dgx pull` (#511)

## Problem

`newt` can already *consume* a vLLM endpoint: `EndpointKind::Vllm` exists in
`newt-core/src/dgx.rs`, `[dgx].nodes[].vllm` holds its URL, `NEWT_DGX_VLLM_URL`
overrides it, and `newt-inference`'s `LocalVllmBackend` speaks its
OpenAI-compatible `/v1` surface. What's missing is the **lifecycle**: nothing
*starts* a vLLM server, picks the right flags for the model and the hardware, or
tears it down. Today that's a hand-run `vllm serve …` over SSH, with the operator
guessing tensor-parallel size, dtype, KV-cache fraction, and max context — and
discovering only at request time that the model didn't fit.

vLLM is the right runtime for the DGX's NVFP4 models (the `nvidia/*-NVFP4`
checkpoints are vLLM/SGLang/TensorRT-LLM format, not GGUF — Ollama can't load
them yet). `newt dgx pull` solved "get GGUF weights onto the node for Ollama";
this solves "stand up a vLLM server for the weights Ollama can't run."

## Hardware reality (probed on dgx1, REDACTED-IP, 2026-06-21)

These shaped the design and differ from a normal discrete-GPU box:

| Fact | Consequence for the tool |
|------|--------------------------|
| **GB10, compute cap 12.1 (Blackwell sm_121)** | NVFP4 is a first-class target; emit `--quantization modelopt_fp4` for NVFP4 checkpoints. |
| **Unified memory; `nvidia-smi memory.total` = `[N/A]`** | The fit pre-flight must size against **system RAM** (`free`/`/proc/meminfo`), *not* `nvidia-smi`. Reuse `dgx_pull::parse_free_bytes`, not a VRAM probe. This is the single most important deviation from upstream vLLM sizing guides. |
| **~117 GiB usable, single GB10** | Default `--tensor-parallel-size 1`. TP>1 is meaningless on one unified device; refuse it with a clear message. |
| **vLLM 0.21.0 already pip-installed** | Prefer the native (`vllm serve`) path; don't assume a container. But probe the version — NVFP4 on Blackwell wants ≥0.22.x (the DeepSeek-V4 recipes pin `0.22.1rc…`). Warn + name the floor when the installed version is too old for the requested dtype. |
| **Docker present but SSH user lacks socket access** | Container path is opt-in (`--runtime docker`), and the tool must *detect* the permission gap and say so, not fail opaquely. Native is the default runtime. |
| **Ollama owns :11434; vLLM default :8000** | Ports never collide; stay on 8000 (matches `synth_from_host` / `NEWT_DGX_VLLM_PORT`). Refuse to start if the port is already bound and it isn't our server. **But port coexistence is not memory coexistence** — both engines draw from the *same* 117 GiB unified pool, and neither negotiates. See "Cross-engine GPU contention" below. |
| **The fit probe must read `MemAvailable`, not `MemTotal`** | `free`'s total column is blind to whatever Ollama already holds resident. On unified memory `MemAvailable` (`free -b` col 7 / `/proc/meminfo`) *already* reflects the other engine's footprint, so sizing against it makes the pre-flight cross-engine-honest for free. The reused `dgx_pull::parse_free_bytes` reads col 2 (total) today — that's the one deviation to fix, not inherit. |

## Surface

A nested subcommand group under the existing `DgxCmd`, keeping all vLLM verbs
together (mirrors how `pull`/`rm`/`ps` cluster the Ollama-lifecycle verbs):

```
newt dgx vllm up <model>   # plan, fit-check, render flags, launch, wait for /health
newt dgx vllm down         # stop the server we started (PID/cgroup tracked)
newt dgx vllm ps           # is it up? which model? GET /v1/models + /health
newt dgx vllm config <model>   # print the resolved launch plan; write nothing
newt dgx vllm logs [-f]    # tail the server log
```

`up` flags (all optional; every one has a derived default):

```
--node <name>              # SSH target (defaults to active node)
--dtype <auto|nvfp4|fp8|bf16|awq|gptq>   # default: inferred from the checkpoint
--max-model-len <N>        # default: derived from fit headroom (see below)
--gpu-memory-utilization <0..1>          # default 0.90
--tensor-parallel-size <N> # default 1; >1 refused on a single unified device
--port <N>                 # default 8000
--served-model-name <id>   # default: the checkpoint's short name
--runtime <native|docker>  # default native
--extra "<raw vllm args>"  # escape hatch, appended verbatim
--force                    # proceed past a fit refusal (the pull --force twin)
--dry-run                  # print the SSH argv + remote script, run nothing
--yes                      # skip the launch confirmation
```

On success `up` offers to persist the endpoint: write `nodes[].vllm =
http://<host>:<port>` and set `active_endpoint = "vllm"`, `active_model =
<served-model-name>` via the same `Config::save` path `dgx setup` uses — so
`newt dgx status` / `route` / `LocalVllmBackend` immediately resolve it. Never
auto-written; gated behind the confirmation like `setup`.

## Architecture — mirror `dgx_pull` exactly

The non-negotiable pattern from #511: **pure logic in its own module, all
SSH/HTTP execution injected, so tests never touch the network or a real node.**

```
newt-cli/src/dgx_vllm.rs      # PURE: plan, fit-check, flag rendering, argv/script
newt-cli/src/dgx.rs           # thin: DgxCmd::Vllm dispatch + injected executor
newt-core/src/dgx.rs          # (already has EndpointKind::Vllm + vllm URL field)
```

Pure surface (every function deterministic and unit-tested, no IO):

```rust
pub enum VllmRuntime { Native, Docker }

pub enum Dtype { Auto, Nvfp4, Fp8, Bf16, Awq, Gptq }
pub fn infer_dtype(checkpoint: &str, hf_quant_config: Option<&serde_json::Value>) -> Dtype;

pub struct VllmPlan {
    pub model: String,
    pub served_name: String,
    pub dtype: Dtype,
    pub tensor_parallel: u8,
    pub max_model_len: u32,
    pub gpu_mem_util: f32,
    pub port: u16,
    pub runtime: VllmRuntime,
    pub extra: Vec<String>,
}

/// Reuse the pull fit-check verb against SYSTEM RAM (unified memory).
pub fn vllm_fit_check(weight_bytes: u64, ram_bytes: Option<u64>, gpu_mem_util: f32) -> FitVerdict;

/// Headroom-aware context default: shrink max_model_len until weights +
/// KV-cache(max_model_len) fit under gpu_mem_util * ram. Pure arithmetic.
pub fn derive_max_model_len(weight_bytes: u64, ram_bytes: u64, gpu_mem_util: f32, requested: Option<u32>) -> u32;

pub fn render_vllm_argv(plan: &VllmPlan) -> Vec<String>;        // `vllm serve …`
pub fn vllm_remote_script(plan: &VllmPlan, log_path: &str) -> String;  // nohup + pidfile
pub fn vllm_docker_argv(plan: &VllmPlan) -> Vec<String>;        // vllm/vllm-openai image
// ssh_argv + parse_free_bytes are reused from dgx_pull (no duplication).
```

The CLI layer holds the only `async` IO and is injected the same way `pull` is:
an SSH executor closure and an HTTP `/health` poller, both fakeable in tests.

### Fit pre-flight — the GLM-5.2 lesson, ported

`pull` refuses a model whose on-disk size exceeds node RAM. `vllm up` is
stricter because a *server* must also hold the KV cache and activations, not
just the weights:

1. Probe weight size (HF API `safetensors` sizes for `nvidia/*` repos; local dir
   size for an already-downloaded path).
2. `vllm_fit_check(weight_bytes, ram_bytes, gpu_mem_util)` — refuse when
   `weight_bytes > gpu_mem_util * ram` unless `--force`. Same `FitVerdict`
   (`Fits`/`Exceeds`/`Undetectable`) and `should_refuse()` the pull path uses.
3. If weights fit but leave no room for context, `derive_max_model_len` clamps
   the window and the plan *says so* ("requested 256K, clamped to 96K to fit
   13 GiB KV under 105 GiB budget") rather than OOM-ing at first request.

This is why the four NVFP4 models the operator asked about resolve the way they
do, and the tool encodes that judgment instead of leaving it to the operator:

| Model | Weights (NVFP4) | `vllm up` verdict |
|-------|-----------------|-------------------|
| `nvidia/Qwen3.6-35B-A3B-NVFP4` | ~22 GB | **Fits** — comfortable context headroom. The realistic target. |
| `nvidia/DeepSeek-V4-Flash-NVFP4` | ~150 GB | **Exceeds** 117 GiB — refuse without `--force`. |
| `nvidia/DeepSeek-V4-Pro-NVFP4` | 1.6T params | **Exceeds** by ~7× — refuse; datacenter (GB300 TP4) only. |
| `nvidia/Kimi-K2.6-Eagle3` | n/a | **Reject at parse** — it's an EAGLE3 *draft head*, not a servable model. Suggest `--speculative` wiring on the future Kimi base instead. |

### The 1M-context models — where weights are *not* the constraint

The operator's headline ask is a **1M-token context** for evaluation. At that
window the **KV cache dwarfs the weights**, and the fit story inverts — a "small"
model can still blow the budget. These are the realistic single-GB10 candidates
(Apache-2.0, reachable in *both* engines — see "Same model, two engines" below):

| Model | Weights | KV @ 1M (bf16) | `vllm up` verdict on 117 GiB |
|-------|---------|----------------|------------------------------|
| `Qwen/Qwen2.5-7B-Instruct-1M` | ~15 GB (bf16) / ~5 GB (fp8) | ~120 GB *full* | **Fits only with a clamped window** — true 1M needs ~120 GB and leaves no headroom; `derive_max_model_len` clamps (≈256–512K at fp8 KV) unless the operator accepts a near-exclusive node. The realistic 1M eval target. |
| `Qwen/Qwen2.5-14B-Instruct-1M` | ~28 GB (bf16) | ~320 GB *full* | **Exceeds at 1M** — upstream's own floor is ~320 GB total VRAM. Servable only at a clamped window (≈128–256K); refuse the full 1M without `--force`, and `--force` will still clamp. |

Two consequences the tool must encode, both falling out of the existing
`derive_max_model_len` + fit-check:

- **1M needs DCA, and DCA has a version floor** — Qwen2.5-1M reaches 1M via
  **Dual Chunk Attention**, originally a custom QwenLM vLLM branch and only later
  upstreamed. Treat "1M on this checkpoint" exactly like NVFP4-on-Blackwell:
  probe the installed vLLM, and if it lacks DCA support for the requested
  `--max-model-len`, **warn + name the floor** rather than launching a server
  that silently caps the window. Emit the DCA rope-scaling override
  (`--hf-overrides`/`--rope-scaling`) the checkpoint's model card specifies.
- **At ≥~512K the KV cache alone consumes the node** — so a 1M (or near-1M)
  vLLM server is, in practice, *exclusive*: nothing else (least of all an Ollama
  model) can co-reside. This is the concrete justification for the lease below.

### Same model, two engines (the "available in both contexts" requirement)

The operator wants each 1M model usable from **both** Ollama and vLLM. There is
no single artifact that satisfies both — **GGUF is Ollama-only, the NVFP4/FP8
safetensors checkpoints are vLLM-only** — so "the same model in both engines"
means *two checkpoints of the same logical model*, and the config must represent
that pair honestly:

- **vLLM side** serves the original `Qwen/Qwen2.5-*-Instruct-1M` safetensors and
  is the **only path to the true DCA 1M window**.
- **Ollama side** serves a GGUF quant of the same model (e.g. the community
  `qwen2.5-1m` tags). llama.cpp's long-context path does **not** reproduce the
  full DCA 1M; Ollama runs the same model at a **clamped window (≈128–256K)**.
  The tool must not imply context parity between the two engines.

Design impact: a logical-model → {engine, checkpoint, max\_ctx} mapping so the
eval harness can flip engines for "the same model" and `dgx` reports the *true*
context ceiling per engine rather than a single optimistic number. This is a
small addition to the `[dgx]` model registry, not a new subsystem.

## Lifecycle & idempotency

- **Process tracking:** native path writes a pidfile + log under
  `~/.newt/dgx/vllm/<served-name>.{pid,log}` on the node. `down`/`ps`/`logs`
  read those; no global "kill all python" guesswork.
- **Idempotent `up`:** if a healthy server for the same model+port is already
  responding on `/v1/models`, `up` is a no-op that reports the existing
  endpoint. A *different* model on the port → refuse unless `--force` (which
  stops the incumbent first).
- **Readiness:** after launch, poll `GET /health` then `GET /v1/models` with a
  bounded backoff; surface vLLM's own startup log on timeout (cold model load
  can be minutes). `--dry-run` prints the argv/script and exits before any of
  this.
- **Single-flight with `warm`:** vLLM has no Ollama-style `keep_alive`; the
  server *is* the residency. `dgx warm` stays Ollama-only; document that for
  vLLM "warm" == "up".

## Cross-engine GPU contention (Ollama ↔ vLLM)

The single most important behavior the original draft missed. On a discrete-GPU
box you might run both engines and let them fight over leftover VRAM; on the
unified-memory GB10 there is **one 117 GiB pool**, both engines draw from it, and
**neither knows the other exists**:

- **vLLM** claims `gpu_memory_utilization × free` *at startup* and holds it for
  the server's entire life.
- **Ollama** allocates lazily per model and releases after `OLLAMA_KEEP_ALIVE`
  (5 min default).

So two silent failure modes the port-coexistence row does not prevent:

1. Ollama has a model resident → vLLM starts against shrunken free memory, sizes
   its KV cache small or OOMs at first request.
2. vLLM holds 90% → the next `ollama run`/`pull`-then-run OOMs.

And for the 1M eval target the pool is effectively *fully* consumed by one
server (see the KV-cache note above), so the engines are mutually exclusive in
practice. The design therefore treats the GB10 GPU as a **single exclusive
resource brokered by newt**, not a shared one.

### The fix is mostly the fit pre-flight, done across engines

Contention-avoidance is not a new subsystem — it's the existing fit-check
measuring *actually-free* memory plus a thin residency record:

1. **Size against `MemAvailable`.** Switch the probe from `free` col 2 (total)
   to col 7 (available) / `/proc/meminfo MemAvailable`. On unified memory that
   figure already nets out Ollama's resident footprint, so `vllm_fit_check`
   becomes cross-engine-honest with a one-line change. The refusal gains a real
   cause: *"won't fit — 38 GiB resident (Ollama: qwen3-coder); `ollama stop` or
   `--force`."*

2. **A cross-engine residency view.** A node-local record
   `~/.newt/dgx/owner.json` (engine, model, claimed-bytes, pid) plus a read-only
   verb:

   ```
   newt dgx gpu        # merged residency: Ollama GET :11434/api/ps (loaded
                       #   models + sizes) ⊕ vLLM /v1/models ⊕ MemAvailable
   ```

   `vllm up` writes the record on success and clears it on `down`; the Ollama
   lifecycle path (pull/run/warm) consults and updates it too. This is the
   "single-flight" the draft scoped *within* vLLM, lifted to span both engines.

3. **An explicit, gated swap for eval ergonomics.** Evicting the other engine is
   never automatic — but the eval loop needs a deterministic flip:

   ```
   newt dgx vllm up <model> --evict-ollama   # ollama stop resident models, then up
   newt dgx run <model> --evict-vllm         # vllm down our server, then ollama run
   ```

   Without the flag, a launch that wouldn't fit alongside the incumbent
   **refuses** and names what to stop — same posture as the GLM-5.2 fit refusal.

### Lease, not lock

The residency record is an **advisory lease**, not a kernel mutex: newt is the
only writer, both lifecycle paths honor it, and a stale record (pid gone) is
reclaimed on read. We deliberately do **not** reach for MPS or MIG — MPS shares
compute but not memory (wrong axis), and MIG is unavailable on the Spark-class
GB10. The honest primitive on this hardware is "one engine owns the device at a
time," and the lease makes that visible and swappable instead of a surprise OOM.

## Testing (matches repo policy)

- Pure module: exhaustive unit tests for `infer_dtype`, `vllm_fit_check`,
  `derive_max_model_len` (boundary cases at exactly RAM, just over, undetectable),
  `render_vllm_argv`, and `vllm_remote_script` quoting — all offline, like
  `dgx_pull`'s tests.
- CLI: injected fake SSH executor + fake health poller assert the right argv is
  built and the persist-confirmation gating works. **No real fs, no real
  network** (per the workspace unit-test-fs policy); a live `vllm serve` smoke
  belongs in the release-gated DGX integration tier, single-threaded, like the
  existing `dgx` live smokes — never in the fast unit suite.
- Contention/lease logic stays pure and fully unit-tested: a `merge_residency`
  fn over fixture `api/ps` + `/v1/models` + `MemAvailable` JSON (no IO), and a
  fit verdict that refuses when a fixture shows an incumbent eating the headroom
  — including the regression case that *would have passed* against `MemTotal`
  and correctly refuses against `MemAvailable`. Eviction is planned purely
  (which models to stop) and executed via the injected SSH closure.

## Out of scope (named, not silently dropped)

- TensorRT-LLM / SGLang backends (vLLM first; the runtime enum leaves room).
- Multi-GPU tensor/pipeline parallel (single GB10 today; refuse TP>1 loudly).
- EAGLE3 / speculative decoding wiring (the Kimi-K2.6-Eagle3 use case) — a
  follow-on once a fitting base model exists.
- Auto-installing/upgrading vLLM on the node. The tool *probes and warns* about
  the version floor for a dtype; it does not `pip install`. (Bootstrapping vLLM
  could be a later `newt dgx vllm install`.)

## Open questions for the operator

1. **Default runtime:** native `vllm serve` (uses the box's 0.21.0, no docker
   perms needed) vs. pinned `vllm/vllm-openai` container (reproducible, but the
   SSH user needs docker-group access first). Proposed default: **native**.
2. **Version floor enforcement:** when installed vLLM < the dtype's floor (e.g.
   NVFP4 on 0.21.0), **hard-refuse** or **warn-and-try**? Proposed: refuse for
   NVFP4 (it genuinely won't load), warn for the rest.
3. **Persist-on-up:** auto-offer to flip `active_endpoint` to `vllm`, or leave
   the operator to `dgx use`/`setup`? Proposed: offer, gated behind `--yes`.
4. **Eviction default:** when a launch won't fit alongside the incumbent engine,
   **refuse-and-name** (operator runs `ollama stop` / passes `--evict-ollama`)
   vs. **auto-evict**? Proposed: refuse by default; eviction only on the explicit
   `--evict-ollama` / `--evict-vllm` flag — never silently kill the other
   engine's resident model out from under a running session.
5. **1M context default:** on a 1M-capable checkpoint, default `--max-model-len`
   to the *clamped* fit-derived window (safe, never OOMs) vs. the checkpoint's
   advertised 1M (honest to the label, but near-exclusive and may refuse)?
   Proposed: clamp by default, with the plan printing "advertised 1M, serving
   <N> to fit — pass `--max-model-len 1048576 --evict-ollama` for the full
   window."
