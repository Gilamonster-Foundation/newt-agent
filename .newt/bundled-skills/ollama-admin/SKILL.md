---
name: ollama-admin
description: Administer Ollama for local-first LLM serving — install and run the service, manage models (pull/list/rm/show/ps/create), tune it with environment variables (host binding, keep-alive, flash attention, KV-cache quant, parallelism), reach a remote Ollama over the network, call it via the native and OpenAI-compatible APIs, register it as a newt backend, and pick a quick-start model by hardware and use case.
when_to_use: When setting up or operating Ollama on a workstation or a remote host; when a model needs pulling, inspecting, or removing; when Ollama must be reachable from another machine; when tuning memory/latency (keep-alive, KV-cache quant, concurrent models); when wiring Ollama into ~/.newt/backends/; or when someone asks "which model should I run" for a given GPU/VRAM budget. For a many-model GGUF library on a DGX Spark / unified-memory box, prefer the llama.cpp router in the dgx-spark-admin skill instead.
version: 1.0.0
license: Apache-2.0
---

# Ollama Administration

Ollama is the low-friction way to run local models: one binary, a pull-based
model registry, a always-on HTTP server, and both a native and an
OpenAI-compatible API. It wraps llama.cpp and trades fine control for
convenience — the right default for a workstation and small remote hosts.

**When NOT to use it:** on a unified-memory box hosting a large library of
models under memory pressure, drive llama.cpp router mode directly (see the
`dgx-spark-admin` skill) — you get per-model flags, explicit residency
control, and upstream architecture support. Ollama is the easy button;
llama-server is the scalpel.

## Service management

Ollama runs as a background server; the `ollama` CLI is a thin client that
talks to it over HTTP (default `127.0.0.1:11434`).

```bash
# Linux (systemd) — installed by the official script
curl -fsSL https://ollama.com/install.sh | sh
systemctl status ollama                 # is it up?
systemctl restart ollama                # after changing config (see below)
journalctl -u ollama -f                 # logs

# Foreground / no systemd (macOS, containers, ad-hoc)
ollama serve                            # runs the server in this terminal

curl -s localhost:11434/api/version     # liveness check
```

**Configuration is by environment variable, not a config file.** For the
systemd service, set them in a drop-in so they survive reinstalls:

```bash
sudo systemctl edit ollama
# In the editor, add:
#   [Service]
#   Environment="OLLAMA_HOST=0.0.0.0"
#   Environment="OLLAMA_KEEP_ALIVE=-1"
#   Environment="OLLAMA_FLASH_ATTENTION=1"
sudo systemctl daemon-reload && sudo systemctl restart ollama
```

For `ollama serve` in a shell, just `export` them first.

## Model management

```bash
ollama pull qwen3-coder:30b       # download (or update) a model
ollama list                       # installed models: name, id, size, modified
ollama ps                         # models currently LOADED in memory (+ VRAM)
ollama show qwen3-coder:30b       # arch, params, context, quant, template, license
ollama show --modelfile <model>   # dump the Modelfile (template + params)
ollama run qwen3-coder:30b        # interactive chat (pulls first if missing)
ollama rm <model>                 # delete
ollama cp <model> <newname>       # copy/alias
ollama stop <model>               # evict from memory now (don't wait for keep-alive)
```

Tags select quant/size: `model:30b`, `model:q8_0`, `model:latest`. Pull the
specific tag you want — `:latest` is not always the quant you'd choose.

**Pull straight from Hugging Face** (any GGUF repo) — these arrive as
canonical GGUFs, which matters if you later migrate to llama.cpp:

```bash
ollama pull hf.co/unsloth/Qwen3-Coder-Next-GGUF:UD-Q4_K_XL
```

### Custom models via Modelfile

A `Modelfile` layers a system prompt, sampler params, or a template onto a
base model:

```dockerfile
FROM qwen3-coder:30b
PARAMETER temperature 0.3
PARAMETER num_ctx 32768
SYSTEM "You are a terse senior engineer. Answer with code first."
```

```bash
ollama create my-coder -f Modelfile
```

`num_ctx` is the lever people miss: Ollama defaults to a **modest context**
(historically 2K–4K) regardless of the model's max. Set `num_ctx` in the
Modelfile (or `PARAMETER num_ctx` / API `options.num_ctx`) to actually use a
long-context model, at the cost of KV-cache memory.

## Tuning (environment variables)

The knobs that matter, roughly in order of impact:

| Variable | Default | What it does |
|---|---|---|
| `OLLAMA_HOST` | `127.0.0.1:11434` | Bind address. Set `0.0.0.0` to serve the network (see security note). |
| `OLLAMA_KEEP_ALIVE` | `5m` | How long an idle model stays resident. `-1` = forever (trade RAM for zero reload latency on an all-day box); `0` = evict immediately. |
| `OLLAMA_FLASH_ATTENTION` | off | `1` enables flash attention — less memory as context grows, and the prerequisite for KV-cache quantization. |
| `OLLAMA_KV_CACHE_TYPE` | `f16` | KV-cache quant. `q8_0` ≈ half the cache memory, negligible quality loss; `q4_0` ≈ quarter, small measurable loss. **Only takes effect with flash attention on.** |
| `OLLAMA_MAX_LOADED_MODELS` | `3×GPU` | How many distinct models may be resident at once. Lower it on a tight memory budget. |
| `OLLAMA_NUM_PARALLEL` | auto | Concurrent requests per model (each needs its own KV slice). |
| `OLLAMA_MODELS` | `~/.ollama/models` | Where blobs live. Point it at a big/fast disk. |
| `OLLAMA_CONTEXT_LENGTH` | model | Default context when a request doesn't set `num_ctx`. |

Memory-saver combo for a shared box:
`OLLAMA_FLASH_ATTENTION=1` + `OLLAMA_KV_CACHE_TYPE=q8_0` +
`OLLAMA_MAX_LOADED_MODELS=1`.

## Remote host management

Two independent things: making a remote server **listen**, and pointing a
**client** at it.

**Serve to the network** — on the remote host set `OLLAMA_HOST=0.0.0.0`
(systemd drop-in above) and restart. Confirm from another machine:

```bash
curl -s http://REMOTE_HOST:11434/api/version
```

**Drive a remote server from your CLI** — the `ollama` client honors
`OLLAMA_HOST` too:

```bash
export OLLAMA_HOST=http://REMOTE_HOST:11434
ollama list                        # now lists the REMOTE server's models
ollama run gemma4:27b              # runs remotely; nothing loads locally
OLLAMA_HOST=http://REMOTE_HOST:11434 ollama pull <model>   # one-off form
```

**Security:** Ollama has **no authentication**. `0.0.0.0` on an untrusted
network exposes an open inference endpoint (and model-management API) to
anyone who can route to it. Keep it behind a LAN/VPN/tailnet, an SSH tunnel
(`ssh -L 11434:localhost:11434 host`), or a reverse proxy that adds auth —
never bind it to a public interface.

## APIs

**Native** (`/api/*`) — Ollama's own shape, with streaming NDJSON:

```bash
curl -s localhost:11434/api/generate -d '{"model":"gemma4:27b","prompt":"hi","stream":false}'
curl -s localhost:11434/api/chat -d '{"model":"gemma4:27b","messages":[{"role":"user","content":"hi"}],"stream":false}'
curl -s localhost:11434/api/tags        # installed models (JSON; same as `ollama list`)
curl -s localhost:11434/api/ps          # loaded models (JSON; same as `ollama ps`)
```

**OpenAI-compatible** (`/v1/*`) — point any OpenAI SDK at
`http://host:11434/v1` with any dummy API key:

```bash
curl -s localhost:11434/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gemma4:27b","messages":[{"role":"user","content":"hi"}]}'
```

The `options` block on the native API is where per-request tuning lives:
`{"options":{"num_ctx":32768,"temperature":0.3,"num_gpu":99}}`.

## Quick-start models by VRAM & use case

Sizes are approximate Q4-class download sizes; a model needs its weights
**plus** KV cache resident, so leave headroom above the number shown. Pick by
the memory you can spare, then by the job. (Model landscape moves fast — treat
this as a starting shortlist, `ollama show` to confirm, and check
`ollama.com/library` for newer tags.)

| VRAM / unified budget | General / chat | Coding | Reasoning | Vision | Tiny / edge |
|---|---|---|---|---|---|
| **≤ 8 GB** | `gemma3:4b`, `qwen3:8b` | `qwen2.5-coder:7b` | `deepseek-r1:8b` | `gemma3:4b` (mm) | `gemma4:e2b`, `nemotron-mini:4b` |
| **12–16 GB** | `gemma4:12b`, `qwen3:14b` | `qwen2.5-coder:14b`, `deepseek-coder-v2:16b` | `deepseek-r1:14b` | `gemma3:12b` (mm) | — |
| **24 GB** (one hot model) | `qwen3.6:27b`, `gemma4:26b` | `qwen3-coder:30b`, `devstral-small-2:24b` | `deepseek-r1:32b`, `qwen3:30b` | `qwen2.5vl:32b` | `granite4.1:30b` (long-ctx) |
| **48 GB+** | `qwen3.6:35b` | `qwen3-coder-next` (MoE) | `deepseek-r1:70b` | `qwen2.5vl:72b` | — |
| **96 GB+** (big unified) | `nemotron:70b` | — | `nemotron-3-super:120b` | — | — |

Rules of thumb:

- **MoE models** (`qwen3-coder:30b`, `qwen3-coder-next`) give the best
  quality-per-GB — large total params, few active — and are the sweet spot on
  24–48 GB.
- **Coding-agent default:** `qwen3-coder:30b` (long context, tool-friendly)
  or `devstral-small-2:24b` for agentic flows.
- **Reasoning:** the `deepseek-r1` line emits explicit thinking; budget extra
  output tokens and expect a `<think>` phase.
- **Vision:** you need a multimodal tag (`qwen2.5vl:*`, `gemma3` mm variants);
  a text-only tag of the "same" model won't see images.
- **First pull for a new box:** something in the 8–14 GB band so you get a
  working answer fast, then pull the bigger model in the background.

## Registering with newt

Ollama is a first-class newt backend. Keep `~/.newt/config.toml` lean — add a
drop-in per backend (filename stem = name):

```toml
# ~/.newt/backends/local-ollama.toml
kind = "ollama"
endpoint = "http://127.0.0.1:11434"
model = "qwen3-coder:30b"
tiers = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
```

A remote box is the same with its `http://REMOTE_HOST:11434` endpoint. In a
session, `/model <name>` swaps the model on an Ollama backend live (any tag
`ollama list` shows on that host). For an OpenAI-SDK consumer instead, point
it at `…:11434/v1`.

## Health & ops crib

```bash
systemctl status ollama                  # service state (Linux)
ollama ps                                # what's resident + VRAM used
curl -s localhost:11434/api/version      # liveness
journalctl -u ollama --since '10 min ago'
ollama stop <model>                      # free memory now
du -sh ~/.ollama/models                  # disk used by blobs
OLLAMA_HOST=http://REMOTE:11434 ollama list   # inspect a remote host
```
