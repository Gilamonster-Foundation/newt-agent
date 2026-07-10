# The DGX Unboxing Experience — project plan

> **North star:** Plug in your DGX, launch newt, and it sets the box up for you.

A DGX Spark is sold as an appliance, but turning it into a model that actually
answers — with tool-calling, tuned to the hardware — is still an expert sport.
newt already carries the pieces to cross that gap. This plan turns them into a
guided **unboxing** and **repair** experience for people who aren't power
developers, and wires it into the first-run splash.

Companion issues: **#1051** (dgx unbox/repair wizards), **#1048** (`setup <url>`
probe), **#<failover>** (backend fallback chain). Pitch page (private artifact):
the "DGX Unboxing Experience" one-pager.

## Why now — proven live

This plan is grounded in a real end-to-end bring-up of **Ornith-1.0-35B (NVFP4)**
on a **GB10 / DGX Spark** (128 GB unified memory), where every failure below was
hit and hand-fixed. That catalogue *is* the wizard's spec.

| What you see | Root cause | What the wizard should do |
|---|---|---|
| `400 "auto" tool choice requires --enable-auto-tool-choice` on the first tool turn | vLLM launched without tool-calling | Emit the parsers from the model card — Ornith → `qwen3` family → `qwen3_xml` / `qwen3` |
| newt hangs `↻ connection lost — retrying…` then errors | Server never bound the port (crashed / OOM) | Detect port-not-listening, read the server log, name the real reason |
| `ValueError: Free memory < desired utilization (0.90)` → `Engine core initialization failed` | Default `--gpu-memory-utilization` too high for **shared unified memory**; an orphaned server also hogging ~20 GB | Hardware-aware util (leave OS + Ollama headroom); reap orphans (SIGKILL the stubborn ones) before launch |
| vLLM won't start after a script edit | Swapped to the 66 GB **FP16** weights on a Blackwell box | Prefer the **NVFP4** (Blackwell-native) quant; warn when a weight set won't fit |
| model id points at nothing | Card says `Ornith-1.0-35B`, vLLM serves `…-NVFP4`; config had `ornith:35b` (exists nowhere) | Reconcile card ↔ served-name ↔ backend `model` from live `/v1/models` |
| `doctor` flags a working vLLM `HTTP 404` | Health check hits the Ollama `/api/tags` path for every backend | Kind-aware probe: `/v1/models` for `kind = "openai"`; probe tool-calling too |
| context capped tiny | `--max-model-len` guessed conservatively | Measure the KV cache (here 3.25M tokens) → set the largest that fits: **262144**, at zero extra memory |

## The bones already exist

We're productizing, not inventing. newt already ships the primitives:

- `dgx vllm up` — stands up a server **from a model card**; stops the running
  one; `--evict-ollama` frees the shared unified pool; polls until serving.
- `dgx switch vllm|ollama <model>` — toggles the session between engines.
- `dgx deploy` — picks the strongest model that **fits** the node (memory
  pre-flight).
- `dgx card` — model cards: every serving knob (parsers, context, footprint) as
  **data**, family-layered (`qwen3` defaults under a specific card).
- `dgx gpu` — reads the accelerator (the hook for detecting unified memory).
- `dgx doctor` — probes every endpoint.

The specific gap this session found: `VllmPlanArgs.gpu_mem_util` defaults to
**0.90** — the exact value that starves a shared unified-memory box (reserving
~97 GB leaves ~2 GB free, so the DGX's own Ollama can't co-load a model).

## The plan — six phases

### Phase 0 — Harden the primitives
Hardware-aware vLLM planning: detect unified memory (GB10/DGX Spark), compute a
`--gpu-memory-utilization` that leaves an OS + Ollama reserve, pick the fitting
quant, size `--max-model-len` from the **measured** KV cache, and make
`vllm up`'s "stop the running server" robust (SIGKILL a SIGTERM-ignoring
process; wait until memory is actually released before launching). Fix `doctor`'s
openai probe path (`/v1/models`, plus a tool-calling check).

### Phase 1 — `newt dgx unbox` (guided setup)
Bare box to working chat, interactive by default, `--yes` scriptable: discover
the node → suggest the best-fitting model → stand it up from its card with
hardware-aware memory → reconcile config + card drop-ins from live `/v1/models`
→ **verify a real `tool_choice:auto` call** before declaring success → land in a
session. Narrates the *why* at each step. Composes with #1048 `setup <url>`.

### Phase 2 — `newt dgx repair` (diagnose & fix)
`doctor` reports; `repair` fixes. It recognizes each failure signature above —
parsing vLLM's real root cause, not the wrapper `Engine core initialization
failed` — and applies the remedy behind a plain-language confirm: reap, retune,
add serve flags, swap FP16→NVFP4, correct a phantom id, fix the doctor probe.

### Phase 3 — Splash onboarding (the dream)
First launch detects an accelerator with no working backend and offers *"Set up
your DGX."* One prompt runs the unbox wizard and drops the user into chat.
TTY-safe and `--no-splash`/headless-safe — never the "press any key" freeze.

### Phase 4 — The unboxing narrative (polish)
Plain-language throughout, written from the owner's side of the screen.
Anonymized failure signatures feed back to sharpen the wizard's remedies over
time — the catalogue grows as more DGXes are unboxed.

### Phase 5 — Productization (partner)
An NGC-container serving path alongside bare-metal, a DGX Spark playbook, and a
co-marketable story: the tribal knowledge of running models on a DGX, encoded
once, carried by every install.

## Design principles

1. **Honest gates** — "setup complete" means a real tool-call came back, not
   "the process started."
2. **Plain language** — every step names what it's doing and why; no CUDA flags
   in the owner's face unless they ask.
3. **Knowledge as data** — serving recipes live in model cards, not code; a new
   model/family is a card, not a patch.
4. **Reversible** — back up before every edit; keep the prior launch script.
5. **Fully mockable** — SSH, HTTP, `nvidia-smi`, `/v1/models` behind seams; the
   unit tier stays fully mocked (the `RecordingSsh` pattern already in `dgx.rs`).
6. **Composes, not rewrites** — the wizards call primitives that already exist;
   each stays usable on its own.

## Success metrics

- Time-to-first-token from a bare box.
- Share of setups that pass the tool-calling probe on the first try.
- Failure signatures auto-repaired vs. escalated to a human.
