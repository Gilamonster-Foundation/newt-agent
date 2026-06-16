# Hierarchical context scheduler — time-sharing one engine across many agents

**Status:** Spec only (Phase 22 not yet built). Design captured for review.
**Naming:** this doc uses dry role names. Component codenames live only in code
comments during development — for traceability: `global-scheduler`≈Leviathan,
`kv-warden`≈Basilisk, `distributed-scheduler`≈Hydra, `execution-worker`≈Wyvern,
`tenant-context`≈Broodmother, `per-model-strategy`≈Dragonlord,
`model-thread`≈snake.
**Related:** [`model-self-tuning.md`](model-self-tuning.md) §4b (Step 20.3 —
fail-open, *never halt*); [`paged-compaction.md`](paged-compaction.md)
(Step 20.4 — *never lose*; the swap mechanism this scheduler invokes);
`docs/decisions/mesh_integration.md` (the `newt-mesh` / `agent-mesh-bus`
substrate the tiers talk over).

## 1. The problem

A small local model handed an oversized context can carve it into chunks and
fan out sub-agents (the map-reduce-over-context pattern). On a fleet with many
GPUs that is embarrassingly parallel. **We have one DGX.** We cannot run N
agents on N accelerators; we must run many *logical* agents on one *physical*
engine, interleaved — the way an operating system time-shares one CPU across
many processes.

This document specifies that scheduler.

## 2. The load-bearing insight: two layers of multiplexing

```
┌─ Layer 2: the AGENT scheduler   ← THIS DOC (the "kernel")
│   admission · placement · priorities · swap; schedules whole agent ROUNDS,
│   yields at tool boundaries
├─ Layer 1: the INFERENCE SERVER  ← already exists (the "CPU + MMU")
│   vLLM continuous batching + PagedAttention, or Ollama NUM_PARALLEL slots
│   physically time-slices the GPU across resident sequences
└─ one DGX
```

**We do not rebuild Layer 1.** PagedAttention is OS-style paging for the
KV-cache, and continuous batching is the time-slicer — the engine already
context-switches sequences every decode step. Reimplementing that is a
multi-year mistake. Phase 22 builds Layer 2: admission control and cooperative
scheduling of logical agents onto a bounded concurrency window, which Layer 1
then physically interleaves.

## 3. The tiers

A two-level hierarchical scheduler (the Borg / Mesos / Kubernetes shape: the
control plane decides *what should happen*, the data plane *makes it happen*).

| Component | Responsibility | Plane | Count | Prior art |
|---|---|---|---|---|
| **global-scheduler** | global view, policy, fairness, placement intent | control | **1** | Borg master / k8s scheduler |
| **kv-warden** | gate workers against the live KV budget; hold waiters; release on free | control | **N** (per pool) | k8s admission / quota |
| **distributed-scheduler** | dispatch / track / rebalance admitted work across engines | data | **M** (per engine) | Borglet / Mesos agent |
| **execution-worker** | run one agent round; yield at the tool boundary | data | **many** | pod / task |

Cardinality is load-bearing: **one** global-scheduler, **many**
kv-wardens / distributed-schedulers (one per pool / per engine), and **swarms**
of execution-workers. A single DGX today is the degenerate flat case
(1 + 1 + 1 + N); a second GPU or node fans the same roles out **without a
redesign**.

**Reuse, don't mint.** The execution-worker reuses the org's existing swarm
drone, `wyvern-agent`. (Note `drake-agent` is a *separate* service — the
quality-grading wing-commander that scores sortie output via quorum — **not** a
worker; it is kept clear of the scheduler.)

### 3.1 Tenants live above the scheduler

The tiers above name *who places work*. *Who wants work* is a tenant — a client
of the global-scheduler, not a scheduler tier (the Borg job/scheduler split):

- **tenant-context** (the map-reduce job) — carves an oversized context into
  chunks, submits a set of execution-workers, and reduces their findings (§6).
- **per-model-strategy** — picks the context strategy per model (summary /
  paged / mapreduce, visible / silent) and feeds it to the global-scheduler as
  policy (§7). The Phase-20 per-model eval writes the learned winner here.

## 4. Mechanism

### 4.1 Agent Control Block (ACB)

The schedulable unit's serializable state — newt's conversation turn-state made
schedulable: `{id, parent, tenant, priority, status, messages, task,
tool_grants, budget(tokens/rounds left), kv_estimate}`. `status ∈ Runnable |
Blocked(on tool) | Admitted | Held | Swapped`.

### 4.2 Cooperative scheduling, quantum = one round

An agentic loop is `generate → tool call → execute → feed → generate`. The tool
call is a **blocking syscall** — the natural yield point. We never preempt
mid-generation (autoregressive decoding can't be cleanly preempted); we
schedule whole rounds, and a worker yields when it emits a tool call (→
`Blocked` while the tool runs on CPU) or finishes. A `max_tokens_per_quantum`
cap is the "timer interrupt" against runaway generation. This is green-threads /
async-await, not preemptive threading.

> *Reserved unit:* if an execution-worker ever runs several context-shards as
> concurrent fibers, those fibers are **model-threads** — minted only if they
> become user-visible; otherwise an implementation detail.

### 4.3 Admission (kv-warden) by KV budget

The scarce resource is VRAM KV-cache, not "GPU time." The kv-warden estimates
each ACB's footprint (`tokens × per-token-KV-bytes`), admits until the pool
budget is full, and **holds** the rest until a slot frees. Critically, the
kv-warden owns the queue **in newt** — admission is capped to the engine's real
concurrency (Ollama `NUM_PARALLEL`, or vLLM's paged-KV pool) so requests never
pile up *opaquely inside the engine*, where priority control is lost.

### 4.4 Placement & the I/O overlap win (distributed-scheduler)

The distributed-scheduler places admitted workers on an engine and drives their
rounds. The win: while worker A is `Blocked` on a `read_file`, its slot frees
and worker B generates — tool I/O on CPU overlaps generation on GPU (classic
multiprogramming). Even on a *serial* engine (one in-flight call) the scheduler
still buys bounded per-call context and forward progress across many agents.

### 4.5 Swap = recompute, not offload (MVP)

Under KV pressure, evict a `Blocked`/`Held` worker's KV-cache but keep its
message list (cheap, on CPU/disk). On resume the engine re-prefills from the
prompt — and **prefix caching makes the shared system-prompt prefix nearly
free**. This is exactly where Step 20.4 composes: *a swapped-out worker's
context is itself paged/compacted*. KV-offload (LMCache / vLLM CPU offload) is a
Phase-2 optimisation, not MVP.

### 4.6 Priority inheritance (no coordinator deadlock)

A tenant blocked on its workers must not stall the user. The worker a
coordinator is blocked on **inherits the coordinator's priority** (textbook
priority-inheritance), with aging to prevent starvation.

## 5. Backend-tier abstraction (both tiers, one seam)

The distributed-scheduler targets an `Engine` trait so the same scheduler serves
both worlds:

- **Serial tier (Ollama / llama.cpp, small `NUM_PARALLEL`)** — matches today's
  stack, no new infra. The scheduler does true OS-style cooperative round-robin
  over 1–N slots; no Layer-1 batching to exploit, so the win is bounded context
  + progress + I/O overlap.
- **Concurrent tier (vLLM / SGLang on the DGX)** — real continuous batching +
  PagedAttention. The kv-warden's budget = the engine's paged-KV pool; the
  distributed-scheduler becomes mostly admission + priority and the engine does
  the physical interleaving. Higher throughput, genuine parallel decode, at the
  cost of operating vLLM.

The tiers differ only behind the `Engine` seam; global-scheduler / kv-warden /
ACB / swap are tier-agnostic.

## 6. First workload: map-reduce over context (tenant-context)

1. The tenant-context carves the oversized context into K chunks (small
   contexts → small KV → many fit).
2. Submits K execution-workers (each: system prompt + chunk + sub-question) to
   the global-scheduler.
3. The kv-warden admits as budget allows; the distributed-scheduler interleaves
   them on the one engine — logically parallel, physically time-sliced.
4. Workers return structured findings; the tenant-context reduces/synthesises.

This is the `Workflow` `pipeline`/`parallel` semantics, but "parallel" now means
"interleaved on one DGX" instead of "needs K GPUs."

**Cross-chunk dependency** is the quality risk (a fact split across two chunks):
mitigate with overlapping windows + a tenant-context join / second-reduce pass.
The eval (§8) must include cross-chunk tasks or the result will look better than
it is.

## 7. Per-model strategy — configurable, optionally silent

Folds into the Phase-20 per-model tuning profile (`community-tunings.toml` /
`CapabilityEntry`) — already per-model and already learnable:

```toml
[[profiles]]
model = "qwen2.5:7b"
context_strategy = "mapreduce"   # summary | paged | mapreduce | auto
strategy_disclosure = "silent"   # visible | silent
```

- **A major behavioural change must be opt-in and per-model** — some models
  reason *worse* under this machinery. Default `context_strategy = "summary"`
  is today's behaviour, bit-for-bit.
- **`silent`** — for models confused by scaffolding, the carve-up / paging
  happens *behind a tool result or beneath the loop*; the model sees an ordinary
  tool output and is never told the context was chunked.
- **`auto`** — the strategy is chosen by the per-model eval (§8): does mapreduce
  beat summary for *this* model's task success? The winner is written back to
  the profile, closing the loop with the self-tuning machinery already shipped.

## 8. Eval plan (deterministic mode required)

Interleaving makes runs non-deterministic. A **deterministic scheduler mode**
(fixed admission order + seed) is required for reproducible evals and bug repro;
ship it alongside a `/workflows`-style live who-ran-when trace.

Compare arms on a fixed suite that *requires* recalling evicted detail and
*spans chunks*:

- **single-agent / no-compaction** (oracle ceiling, fits when it fits),
- **summary** (today's destructive compaction),
- **paged** (Step 20.4),
- **mapreduce** (this scheduler).

Metrics: task-success rate (mapreduce/paged should approach the oracle, beat
summary), cross-chunk-task success, page-fault & swap-thrash rate, anti-thrash
latch rate (~0), wall-clock vs throughput per tier, tokens/round. A strategy
becomes a model's `auto` default only when it wins on success without a thrash
regression — otherwise it stays behind the flag.

## 9. Composition (the whole stack)

**20.3** never halt → **20.4** never lose (page) → **Phase 22** interleave many
paged contexts on one engine → **tenant-context** carve + fan out. Each layer is
independently shippable and each makes the next cheaper.

## 10. MVP scope & co-location

Four tiers is right for the multi-node future and over-engineered for one DGX
today. The Borg lesson: one master per cell — don't deploy the hierarchy until
there are multiple cells.

- **MVP co-locates global-scheduler + kv-warden + distributed-scheduler in one
  process** over the single Ollama/vLLM engine; the four seams exist as traits +
  `newt-mesh` messages but only split across the bus when a **second engine
  appears**.
- Lives in / beside `newt-mesh` (the workspace-excluded, feature-gated crate —
  `mesh_integration.md`), not the default build. It is the natural home of the
  planned "drake-on-mesh parallel dispatcher" (Phase 5).
- Serial tier (Ollama) first — matches current hardware; the concurrent (vLLM)
  tier slots behind the same `Engine` seam later.

### Step breakdown

- **22.1** — `Engine` trait + serial (Ollama) impl; ACB; single co-located
  global-scheduler/kv-warden/distributed-scheduler; cooperative round quantum;
  round-robin + priority inheritance. No swap yet (admission-only backpressure).
- **22.2** — kv-warden KV-budget estimation + hold/release; swap-by-recompute
  with prefix caching (composes Step 20.4).
- **22.3** — tenant-context map-reduce job (carve → submit → reduce) +
  cross-chunk join pass.
- **22.4** — per-model `context_strategy`/`strategy_disclosure` + silent mode;
  deterministic scheduler mode + live trace.
- **22.5** — eval harness (§8); wire `auto` to the Phase-20 writeback.
- **22.6** *(follow-on)* — concurrent (vLLM) `Engine` impl; KV-offload swap;
  multi-engine distributed-scheduler split across the bus.

## 11. Out of scope

- Reimplementing continuous batching / PagedAttention (Layer 1 is the engine's
  job — §2).
- Multi-**node** scheduling (multiple DGXes) — the tier seams anticipate it;
  Phase 22 targets one node.
- KV-offload swap and the concurrent-tier engine — deferred to 22.6.
- Changing default behaviour — `context_strategy = "summary"` stays bit-for-bit
  until the eval (§8) earns a per-model flip.
- Headless surfaces reading/writing scheduler state (mirror
  `model-self-tuning.md` §5: hooks stay `Option`/absent there).
