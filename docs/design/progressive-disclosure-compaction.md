# Progressive-disclosure compaction — context compaction as disclosure, not destruction

**Status:** Spec only (Step 20.4 not yet built). Design captured for review.
**Naming (reconciliation, 2026-06-16):** the innovation is **progressive
disclosure** — context is a budgeted, addressable resource the agent pulls on
demand. "Paging" (window = RAM, store = disk, `memory_fetch` = page-fault) is the
*mechanism metaphor* used below to explain it; the concept name is
progressive-disclosure, and this is the compaction-path application of the same
disclosure substrate as [`progressive-disclosure-memory.md`](progressive-disclosure-memory.md).
**Related:** [`model-self-tuning.md`](model-self-tuning.md) §4b (Step 20.3 —
fail-open, the *never-halt* safety net this feature complements);
[`progressive-disclosure-memory.md`](progressive-disclosure-memory.md) (#319 —
the `memory_fetch` index-then-fetch substrate this feature reuses);
[`model-family-profiles.md`](model-family-profiles.md) (owns the *selector* — the
`compaction_mode` profile knob this doc's mechanism is chosen by).

## 1. The problem with destructive compaction

`compress()` (`newt-core/src/agentic/compress.rs`) reclaims context by
*destroying* it: structural prune → boundary (keep head + tail, drop the
middle) → replace the middle with a lossy LLM summary or a static marker.
The evicted span is gone. Two failure modes follow:

1. **Information loss.** Even a "successful" compaction degrades the model's
   working set — a file it read, a command's output, an earlier decision —
   to a paragraph of prose. Later rounds make worse decisions because the
   detail is unrecoverable.
2. **The halt (the motivating bug).** When a span is incompressible (a large
   system prompt, an already-tiny history where the summary outweighs the
   prune — `model-self-tuning.md` §1.3), a pass reclaims <10%, anti-thrash
   latches after two strikes, and the loop refuses its own send. Step 20.3
   makes that refusal fail open on non-authoritative budgets; it does not
   remove the underlying pressure.

## 2. The reframe: the window is RAM, the store is disk

Treat the context window as RAM, the conversation turn-store as disk, and
`memory_fetch` as the page-fault handler. Compaction stops being `free()`
and becomes `swap out`: the evicted span is written to (or already lives in)
a content-addressed store, and is replaced in-context by a **page-table
entry** — a short index marker carrying a retrieval handle. The model faults
the span back in verbatim, on demand, only when it needs the detail.

The lossy summary does not disappear — it is demoted from *replacement* to
*catalog card*: a one-line gist per evicted span, beside the handle. A model
that never pages is no worse than today's summary compaction; a model that
needs precision pages the verbatim span back.

## 3. What already exists (the surprise: ~80% built)

This feature connects two subsystems newt already ships; it does **not**
introduce a new store.

| Capability | Where | Note |
|---|---|---|
| `memory_fetch` tool, index-then-fetch | `memory.rs:771` | "`use_skill`'s index-then-fetch shape applied to memory" |
| Resolver `turn:<conv>#<seq>` | `store.rs:581`, `store.rs:2045` (`load_turn` by `(conv, seq)`) | verbatim history is **already persisted and addressable** |
| Resolver `note:<id>` | `memory.rs` index | the durable knowledge-bank tie-in |
| Content addressing (blake3) | `store.rs:969` (`canonical_encoding_v1`) | tamper-evident, dedup'd spans |
| Opt-in flag pattern, default inert | `MemoryDisclosure::Frozen`/`Index` (`newt-core/src/config/memory.rs`) | exact "behind a flag, bit-for-bit unchanged by default" shape |
| CI-pinned index budget | `MEMORY_INDEX_BUDGET = 12` (`memory.rs:763`) | "cheap layer rides every request; bodies on demand" |

The gap is narrow and specific: **the compaction marker does not carry the
`memory_fetch` handles.** The keys exist; eviction simply never hands them
to the model.

## 4. Design

### 4.1 The selector — a profile knob, not a standalone facet

The mode is a **profile knob** (`compaction_mode`), not a free-standing
`[compaction]` table: it lives in `ProfileConfig` / `model-family-profiles.md`
so it is per-model/per-family and selectable by a loadout — the kit owns the
*selector*, this doc owns the *mechanism*. Default = today's behavior:

- `summary` *(default)* — the current destructive prune+summary/marker path,
  bit-for-bit unchanged. Inert unless opted in (the `MemoryDisclosure`
  precedent).
- `disclosure` — eviction emits a `memory_fetch`-handle index marker instead of
  a lossy replacement, and ensures `memory_fetch` is wired for the session. (The
  *mechanism* is paging; `disclosure` names the innovation per the banner above.)

### 4.2 The eviction marker

When the boundary step evicts the middle span, for each evicted **persisted**
turn it emits one index line:

```
paged: turn:<conv>#<seq> — <one-line gist>
```

assembled into a single marker message:

> *Earlier turns were paged out to keep the window small. They are preserved
> verbatim — call `memory_fetch` with any handle below to read one back.*
> followed by the budgeted index lines.

The gist reuses the existing summary machinery (cheap orientation); the
handle is the lossless backing. The index is budgeted (the
`MEMORY_INDEX_BUDGET` convention) so the marker itself cannot grow unbounded.

### 4.3 Wiring

`paged` mode implies the `memory_fetch` tool is advertised — reuse the
`MemoryDisclosure::Index` wiring rather than a parallel path. No new store,
no new resolver: `turn:<conv>#<seq>` already resolves.

### 4.4 Hybrid by necessity

Only *persisted* turns have a stable handle. The live in-flight turn and
mid-round tool output may not be committed to the store yet, so those still
take the old prune+summary path. Paged mode is therefore **hybrid**: page
persisted history, summarize the live tail. (Step 20.4 must verify exactly
when `store.rs` commits a turn, to know the pageable boundary.)

### 4.5 Why this also prevents the halt

Anti-thrash latches when a pass reclaims `< THRASH_MIN_SAVINGS` (10%).
Replacing a span with a ~30-token handle is *unconditionally* a large
reclaim → always effective → the breaker never trips → the loop never
refuses. Paged compaction attacks the halt from the **prevention** side;
Step 20.3's fail-open is the **safety net**. Two layers: *never lose*
(paging) and *never halt* (fail-open). The §1.3 "summary outweighed the
prune on a tiny history" pathology cannot occur — a handle is always
smaller than the span it replaces.

## 5. The hard parts (do not pretend these away)

1. **Re-page thrash.** A faulted-in span grows the window → may re-trigger
   eviction → re-evict. Re-eviction is free (same handle), but a model that
   keeps re-fetching the same turn oscillates. Mitigation (Step 20.5): pin a
   recently-paged-in span for *K* rounds, or impose a per-turn page-in token
   budget. This is genuine VM thrashing — measure it.
2. **Small-model tool reliability.** newt targets local models that may not
   reliably call `memory_fetch`. The per-line gist is the floor: a
   non-paging model degrades to exactly today's summary quality. Measure
   page-fault precision/recall in the eval, never assume it.
3. **Pageable boundary.** Mid-turn scratch isn't persisted yet (§4.4) — be
   explicit in the marker about what is and isn't retrievable, so the model
   doesn't fetch a handle that 404s. `memory_fetch`'s contract already
   returns labelled absence rather than erroring; keep that.
4. **Durability & GC.** The store is durable SQLite, so paging from a
   *resumed* conversation is free and powerful — but durable reads need
   TTL/GC eventually. MVP scopes reads to the current session; cross-session
   is Step 20.5.
5. **KV-cache churn.** Compaction already invalidates the cache from the
   boundary down (it rewrites the middle). Paging keeps that property; the
   rule is unchanged: never let paging churn the *head* of the prompt.
6. **Eval honesty.** Gate the claim on a real number (the org's quorum-review
   norm) — do not ship "better" on intuition.

## 6. Scope

### Step 20.4 — paged compaction MVP

- `CompactionMode::Paged` behind `[compaction] mode`, default `summary`.
- Boundary eviction emits the `memory_fetch`-handle index marker (handle +
  gist) for already-persisted turns; reuse `MemoryDisclosure::Index` wiring.
- Reads scoped to the current session.
- Anti-thrash: paged eviction counts as effective (large reclaim) by
  construction — assert it never latches in the paged path.
- Eval harness (§7).

### Step 20.5 — durability & thrash control (follow-on)

- Re-page thrash guard (pin recently-faulted spans / page-in budget).
- Durable cross-session paging + GC/TTL.
- Promotion: a span faulted repeatedly across sessions is promoted from the
  ephemeral turn-store to a durable knowledge-bank `note:<id>` — the
  knowledge-bank tie-in, closing the loop with the user's local CRM/planner.

## 7. Eval plan

This is an **arm of the existing ground-truth rig (#75) + model sweep (#350)**
feeding the Phase-20 writeback — not a standalone harness. The
`compaction_mode` winner is written back to the model card / profile
(`tune_source`-tagged so a swept value never clobbers a hand-authored one),
the same fitness function `model-family-profiles.md` defines.

Compare three arms on a fixed task suite that *requires* recalling evicted
detail (read a large file early, use a fact from it many rounds later):

- **no-compaction** (oracle ceiling — fits when it fits),
- **summary** (today's destructive path),
- **disclosure** (this feature — paged eviction).

Metrics:

- **task success rate** (paged should approach the oracle, beat summary);
- **page-fault rate** — fraction of evicted spans the model faults back in
  (too low ⇒ marker not imperative enough; too high ⇒ thrash);
- **thrash rate** — re-evictions of a just-paged span per turn;
- **anti-thrash latch rate** — must be ~0 in the paged arm (§4.5);
- **tokens/round** — paged should hold the window tighter than summary while
  losing less.

Ship `paged` as default only if it beats `summary` on success rate without a
thrash regression. Until then it stays behind the flag.

## 8. Out of scope

- Headless surfaces reading/writing the paging store (mirror
  `model-self-tuning.md` §5: the hooks stay `Option`/absent there).
- Eviction of the *current* turn's un-persisted scratch (§4.4 — summarized,
  not paged).
- Cross-session durability and GC (Step 20.5).
- Knowledge-bank promotion (Step 20.5).
- Any change to the default compaction path — `summary` stays bit-for-bit
  identical until the eval (§7) earns a default flip.
