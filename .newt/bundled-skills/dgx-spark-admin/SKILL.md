---
name: dgx-spark-admin
description: Administer LLM inference hosting on an NVIDIA DGX Spark (GB10) — choose an engine, build llama.cpp for sm_121, serve a multi-model GGUF library with llama-server router mode, migrate models and hosting off Ollama, co-host with vLLM on unified memory, and register served models as newt backends.
when_to_use: When setting up, tuning, or migrating LLM inference on a DGX Spark / GB10 (or similar unified-memory Grace-Blackwell box); when a vLLM serve is soaking unified memory and another model must co-host; when replacing an Ollama deployment with llama.cpp; when a new GGUF model needs to be served; when wiring a served model into ~/.newt/backends/.
version: 1.0.0
license: Apache-2.0
---

# DGX Spark (GB10) Inference Administration

The DGX Spark is not a datacenter GPU box and must not be administered like
one. Three platform facts drive every decision below:

1. **Unified memory.** CPU and GPU share one 128 GB LPDDR5X pool. `nvidia-smi`
   reports (nearly) the whole pool as "GPU memory". The OS, your shell, every
   inference engine, and every model's weights + KV cache all compete for the
   same bytes. There is no "system RAM is separate" safety margin.
2. **aarch64.** The Grace CPU is ARM. x86 wheels, binaries, and Docker images
   do not run. Prefer building from source or using aarch64/sbsa artifacts.
3. **Blackwell sm_121, CUDA 13.** The GB10 GPU needs CUDA 13.x and
   `sm_121` codegen. Prebuilt binaries compiled for older arches either fail
   or silently run unoptimized. Blackwell has native 4-bit paths — Q4 GGUF
   quants and NVFP4 are the performance sweet spots.

## Choosing an engine

| Engine | Model format | Strengths on Spark | Costs |
|---|---|---|---|
| **llama.cpp** (`llama-server`) | GGUF | Q4-optimized on Blackwell; mmaps only what the model needs; native **router mode** = multi-model, load-on-demand; multimodal via `mmproj` | Build from source; new architectures land days after vLLM |
| **vLLM** | HF safetensors (incl. NVFP4/FP8) | Day-0 support for new models; best throughput under concurrent load; production batch serving | **Pre-allocates** `--gpu-memory-utilization` × visible memory at startup — on unified memory the default (~0.9) starves everything else; nightly builds needed for newest models |
| **Ollama** | GGUF (wrapped llama.cpp) | Easy pulls | A wrapper: less control over offload, context, templates, and per-model flags. llama-server router mode now covers the "many models, load on demand" use case natively — prefer it for hosting. Migration path below. |

Rules of thumb:

- **One hot model under concurrent load** → vLLM with an explicit, budgeted
  `--gpu-memory-utilization`.
- **A library of models, switched on demand** → llama-server router mode.
- **Both at once** → co-host (section below); give vLLM a fixed share and let
  llama.cpp live in the remainder.

## Building llama.cpp for the Spark

Requires CUDA 13.x (`ls /usr/local/ | grep cuda`) and a normal build
toolchain. Target sm_121 explicitly:

```bash
git clone https://github.com/ggml-org/llama.cpp
cd llama.cpp
cmake -B build -DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=121
cmake --build build --config Release -j"$(nproc)"
build/bin/llama-server --version   # sanity check
```

If `nvcc` is not on `PATH`, prefix the cmake configure with
`CUDACXX=/usr/local/cuda-13/bin/nvcc`. Install by copying `build/bin/llama-*`
to `/usr/local/bin` or point service units at the build directory.

## Serving a model library (router mode)

Router mode is the Ollama replacement: point `llama-server` at a directory of
GGUFs; models load on first request and are LRU-evicted when `--models-max`
is exceeded. Each model runs in its own process, so one crash doesn't take
down the server.

```bash
llama-server \
  --models-dir /srv/models/gguf \
  --models-max 2 \
  --host 0.0.0.0 --port 8080 \
  -ngl 99 --jinja -c 32768
```

- `--models-max` is your memory governor. On unified memory keep it **low**
  (1–2) — N resident models × their GGUF sizes must fit your budget.
- `-ngl 99` offloads effectively all layers; on unified memory there is no
  reason to hold layers back.
- `--jinja` uses the chat template embedded in each GGUF (essential for
  migrated Ollama models — see below).
- Clients select a model with the standard OpenAI `model` field
  (`"model": "my-model.gguf"`). `GET /models` lists discovery + load state;
  `POST /models/load` / `/models/unload` control residency manually.

Per-model overrides (context size, sampler defaults, **multimodal `mmproj`
pairing**) go in a preset file:

```ini
# models.ini — pass with --models-preset models.ini
[glm-4.6v-flash]
model = /srv/models/gguf/glm-4.6v-flash.gguf
mmproj = /srv/models/gguf/glm-4.6v-flash.mmproj.gguf
ctx-size = 65536
```

For a **single dedicated model**, skip router mode: `llama-server -m
model.gguf -ngl 99 --jinja -c <ctx> --port <port>` (add `--mmproj
<file>.gguf` for multimodal models — load BOTH the main weights and the
projector or vision input silently fails).

### Getting GGUFs

Prefer Unsloth quants; `UD-Q4_K_XL` is the quality/speed sweet spot on
Blackwell. Either download explicitly:

```bash
huggingface-cli download unsloth/GLM-4.7-Flash-GGUF \
  --include '*UD-Q4_K_XL*' --local-dir /srv/models/gguf/
```

or let llama-server fetch into its cache: `llama-server -hf
unsloth/GLM-4.7-Flash-GGUF:UD-Q4_K_XL`. Router mode also discovers the
`~/.cache/llama.cpp` cache automatically.

## Migrating off Ollama

Two independent halves: the **models** and the **hosting**. Do the models
first; keep Ollama running until the replacement passes its smoke test. (For
operating Ollama itself — before, during, or instead of this migration — see
the companion **`ollama-admin`** skill.)

### 1. Model migration — Ollama blobs ARE GGUFs

Ollama stores every model as content-addressed blobs under
`~/.ollama/models/blobs/` (or `$OLLAMA_MODELS`); the model-weights blob is a
plain GGUF file that llama.cpp can serve directly. No re-download, no
conversion. The bundled script hardlinks every installed model (and any
multimodal projector) into a normally-named library:

```bash
./ollama-export-gguf.sh /srv/models/gguf
```

Notes:

- **Hardlinks cost zero disk** and survive deletion of the Ollama store —
  but require the destination to be on the same filesystem. The script falls
  back to copying when it isn't.
- **Cloud/remote-only models** (no local weights layer) are skipped — they
  were never local to begin with.
- **Not every Ollama blob loads in upstream llama.cpp.** Models that run on
  Ollama's own engine are converted with Ollama-specific GGUF metadata and
  fail in two recognizable ways: `unknown model architecture: '<name>'` (an
  arch string upstream doesn't know) or `wrong number of tensors; expected
  N, got M` (an Ollama-specific tensor layout). Neither is fixable in place
  — re-pull the HF-canonical GGUF (e.g. the Unsloth quant) for those and
  quarantine the broken export. Models that were pulled via
  `ollama pull hf.co/...` are canonical GGUFs and migrate cleanly.
- **Verify every migrated model, not just one.** Sweep the library with a
  1-line chat request per model and quarantine failures:

  ```bash
  for m in $(curl -s localhost:8080/models | jq -r '.models[].id'); do
    curl -s -m 300 localhost:8080/v1/chat/completions \
      -H 'Content-Type: application/json' \
      -d "{\"model\":\"$m\",\"messages\":[{\"role\":\"user\",\"content\":\"say ok\"}],\"max_tokens\":10}" \
      | grep -q '"content"' && echo "PASS $m" || echo "FAIL $m"
  done
  ```

  Skip models larger than the memory a co-hosted engine leaves free — they
  only load when the co-tenant is stopped (note them as such).
- **Chat templates**: Ollama keeps its own Go-format template in the
  manifest, which does NOT transfer. Serve with `--jinja` so the template
  embedded in the GGUF is used — HF-converted models virtually always carry
  one. Smoke-test each migrated model with a short chat request and inspect
  the reply for template artifacts (leaked role tags, missing stop).
- Sampler defaults from Ollama Modelfiles (`temperature`, `top_p`, …) also
  don't transfer; recreate the ones you care about in the preset file.

### 2. Hosting migration

1. Stand up router mode as a service (unit below), pointed at the exported
   library, on a **different port** than Ollama's 11434.
2. Smoke-test the models you actually use through `/v1/chat/completions`.
3. Repoint clients. Ollama's native API (`/api/generate`, `/api/chat`) is
   gone; everything speaks OpenAI (`/v1/*`) now — most tooling just needs a
   new base URL. (llama-server also serves Anthropic-style `/v1/messages`.)
4. Decommission: `sudo systemctl disable --now ollama`. Once satisfied, the
   Ollama store can be removed — hardlinked exports keep the weights alive.

```ini
# /etc/systemd/system/llama-router.service
[Unit]
Description=llama.cpp router — multi-model OpenAI-compatible server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/llama-server \
  --models-dir /srv/models/gguf \
  --models-preset /srv/models/models.ini \
  --models-max 2 \
  --host 0.0.0.0 --port 8080 \
  -ngl 99 --jinja -c 32768
Restart=on-failure
# Run as a dedicated user that owns /srv/models
User=llama

[Install]
WantedBy=multi-user.target
```

## Co-hosting with vLLM on unified memory

vLLM pre-allocates its share at startup; llama.cpp takes only what its
resident models need. That asymmetry dictates the procedure:

1. **Budget on paper first.** With `T` = total unified memory:
   `vLLM share (util × T)` + `Σ resident GGUF sizes + their KV` +
   `~8–10 GB OS/services` ≤ `T`, with real headroom left.
2. **Always set `--gpu-memory-utilization` explicitly.** The default (~0.9)
   is sized for a dedicated GPU and will starve the box. On a shared Spark,
   0.5–0.65 is the realistic range. Note vLLM's KV capacity comes from this
   share, not from `--max-model-len` — lowering util shrinks concurrent
   context capacity, usually harmlessly.
3. **Start vLLM first** (it grabs its fixed share), llama.cpp after (it
   flexes into the remainder).
4. **Verify** with `free -h` — leave several GB truly free; unified memory
   exhaustion manifests as OOM-kills and desktop lockups, not graceful
   errors.

Worked example on a 128 GB Spark: vLLM at `0.60` ≈ 73 GB, one resident
~20 GB Q4 GGUF + ~4 GB KV in llama-server (`--models-max 1`), ~10 GB
OS/services → ~107 GB committed, ~14 GB headroom.

## Registering with newt

Serve endpoints become drop-in backends — one file per backend, filename
stem is the name (keep `~/.newt/config.toml` lean; tokens in files, never
inline):

```toml
# ~/.newt/backends/spark-router.toml
kind = "openai"
endpoint = "http://spark-host:8080"
# model omitted on purpose: newt probes the server and adopts what it
# serves at session start. Pin one only if the router hosts many and you
# want this backend to mean a specific model:
# model = "glm-4.7-flash.gguf"
# api_key_file = "~/.newt/tokens/spark-router"   # if the endpoint is fronted with auth
```

A co-hosted vLLM gets its own sibling drop-in pointing at its port.

## Health & ops

```bash
curl -s localhost:8080/health            # liveness
curl -s localhost:8080/models | jq       # router: discovery + load states
curl -s localhost:8080/v1/models | jq    # OpenAI-style listing (vLLM too)
nvidia-smi                               # unified pool usage
free -h                                  # the number that actually matters
journalctl -u llama-router -f            # service logs
build/bin/llama-bench -m model.gguf      # tokens/sec baseline after changes
```

Record a `llama-bench` baseline after every llama.cpp rebuild or driver
update — regressions on sm_121 have historically come from builds targeting
the wrong arch, and the bench catches that immediately.
