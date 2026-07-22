---
name: dgx-model-switch
description: Switch a DGX Spark (GB10, 128 GB unified) between its default co-hosted mode (vLLM serving a mid-size model + llama.cpp router for small GGUFs) and a "big model" mode that shuts vLLM down to give the whole unified-memory box to one large GGUF (e.g. Nemotron-3-Super-120B, Kimi-Dev-72B). Provides a bundled llm-mode.sh toggle and the commands to run it, add new big models, and rebuild the script on a fresh box.
when_to_use: When someone wants to temporarily run a large GGUF that cannot co-host with the resident vLLM model on a unified-memory Spark; when asked to "switch to <big model>", "shut down the other models to host X", or "go back to normal/Ornith"; when adding a new large model to the switch registry; when the llm-mode.sh toggle is missing on a Spark and must be redeployed.
version: 1.0.0
license: Apache-2.0
---

# DGX Spark Model Switching (co-host ↔ big-model)

Companion to `dgx-spark-admin`, which covers *building* llama.cpp, router
mode, co-hosting, and backend registration. This skill covers the **runtime
switch** between two mutually-exclusive memory configurations on one unified
128 GB box.

## The core constraint

CPU + GPU share one 128 GB pool (see `dgx-spark-admin`). You cannot run a
heavy vLLM serve *and* a large GGUF at once. Two operating modes:

| Mode | :8000 vLLM | :8080 llama-router | Fits |
|---|---|---|---|
| **ornith** (default) | Ornith-35B @ util 0.65 (~79 GB) | small/medium GGUFs co-hosted, ≤~25 GB each | both |
| **big** (e.g. super, kimi-dev) | **DOWN** | ONE large GGUF resident (up to ~100 GB) | one |

Rule of thumb: a GGUF **>~30 GB cannot co-host** — it needs big mode.

## The toggle: `llm-mode.sh`

Bundled with this skill (`llm-mode.sh`). It lives on the Spark at
`~/bin/llm-mode.sh`. Commands:

```bash
llm-mode.sh status        # what's serving now + memory
llm-mode.sh list          # registered big-model modes + whether each GGUF is present
llm-mode.sh ornith        # <- default: evict big model, bring vLLM/Ornith back up
llm-mode.sh super         # -> Nemotron-3-Super-120B (vLLM off)
llm-mode.sh kimi-dev      # -> Kimi-Dev-72B (vLLM off)
llm-mode.sh big <stem>    # -> any GGUF stem in ~/models/gguf/ (vLLM off)
```

In a big mode you query the model at `http://<spark>:8080/v1/...` with
`"model": "<stem>"`. Back in `ornith`, Ornith is at `:8080`'s neighbour
`:8000` as usual.

### Why the order matters (do NOT reorder)

The script encodes two safety rules — replicate them if you ever do this by
hand:

- **→ big:** `stop vLLM` FIRST (frees its ~79 GB reservation), THEN load the
  large GGUF. Loading first will OOM.
- **→ ornith:** `restart llama-router` to EVICT the resident big model BEFORE
  starting vLLM. vLLM pre-allocates its whole `--gpu-memory-utilization`
  share at startup; if the 80 GB GGUF is still resident it OOMs immediately.

`--models-max 1` means the router keeps the last model resident until another
is requested — that is exactly why an explicit evict (router restart) is
needed before handing memory back to vLLM.

## Adding a new big model

1. Get the GGUF into `~/models/gguf/<stem>.gguf` (Unsloth `UD-Q*_K_XL`
   preferred; see `dgx-spark-admin` "Getting GGUFs"). Multi-part quants:
   download all shards and merge to one file so the stem matches:
   ```bash
   ~/src/llama.cpp/build/bin/llama-gguf-split --merge \
     <shard>-00001-of-000NN.gguf ~/models/gguf/<stem>.gguf
   ```
2. Either run it ad-hoc: `llm-mode.sh big <stem>`, or register a friendly
   mode name by adding to the `BIG` associative array near the top of
   `llm-mode.sh`:
   ```bash
   declare -A BIG=(
     [super]="nemotron-3-super_120b"
     [kimi-dev]="kimi-dev_72b"
     [my-mode]="<stem>"
   )
   ```
3. `llm-mode.sh list` confirms the file is present before you switch.

## Rebuilding the script on a fresh Spark

If `~/bin/llm-mode.sh` is missing, redeploy the bundled copy:

```bash
mkdir -p ~/bin
install -m755 "$SKILL_DIR/llm-mode.sh" ~/bin/llm-mode.sh
~/bin/llm-mode.sh status
```

Adjust the constants at the top if the box differs: `GGUF_DIR`, `ORNITH_SH`
(the vLLM launcher), and the port numbers. The script assumes llama-router is
a systemd unit (`llama-router.service`) and vLLM is launched by a shell
script (`~/ornith.sh`) — both per `dgx-spark-admin`.

## Verifying a switch

- `llm-mode.sh status` should show the target up and the other down.
- A big-model load is confirmed by a real chat reply (the script does this
  and prints `OK: '<stem>' loaded and replied`), not just process presence.
- If a big GGUF was an Ollama-engine export it may fail to load upstream
  llama.cpp (wrong-tensor / unknown-arch); re-pull the HF-canonical GGUF
  (see `dgx-spark-admin`). The script surfaces the load error verbatim.
