# Crew MVP — empirical findings (2026-06-16)

The "boring two-pass machine" (`crew.py`), run live against the gnuc+DGX pool. The
crawl step: learn the real behavior before building the Rust `newt-scheduler` crate.

## It works

End-to-end, **first pass, twice** (a `fib` task and a harder subtractive-Roman task).
The navigator picked the right files out of distractors; the planner wrote correct,
**scoped** edits (touched only the target file, left `add`/distractors alone); the
worktree fence held; the test passed. All on real 24B/30B/3B models across two machines.

| signal | result |
|---|---|
| crew completes end-to-end | ✅ (fib + roman, both pass) |
| JSON contract adherence (Ollama `format:json`) | ✅ valid structured JSON every call |
| edit validity (full-content edits) | ✅ applied cleanly, correctly scoped |
| navigation (pick relevant from distractors) | ✅ devstral picked 2 of 4–5 |
| all 3 roles × both machines | ✅ nav devstral@dgx · planner qwen3-coder@dgx · triage qwen2.5-coder:3b@gnuc |

## The numbers (and the design-validating finding)

| run | nav load | planner load | wall | note |
|---|---|---|---|---|
| 1 (cold) | 6.8s | 12.4s | ~30s | one-time cold model-load dominates |
| 2 (warm) | 0.1s | 0.1s | ~11s | **zero loads — both models still resident** |

- **Co-residence → zero per-round swap, confirmed.** The DGX holds devstral-small-2:24b
  *and* qwen3-coder:30b resident simultaneously (`/api/ps`); a devstral re-call after the
  planner ran showed `load_duration=0.4s` — never evicted. So the "place both big models
  on the big box, zero swaps per round" design holds on this hardware.
- **Cost is cold-start, not swapping.** Warm generation is fast (~5s per 30B call, ~1.7s
  triage). The residency scheduler's job here is to keep the big models *warm*
  (`keep_alive`), not to shuffle weights — "place, don't swap" is the right default.
- **gnuc triage runs in parallel with the DGX** (different machine) — the I/O-overlap win
  is available for free.

## Corrections to fold back into the design

- **gnuc (16GB RTX 4060 Ti) CANNOT host a 30B planner.** `crew-loadout.md` /
  synthesis said "planner qwen3-coder:30b (either, failover-able)" — false in practice:
  18.6GB won't fit a 16GB card (and ~10GB was already in use by other work). The real
  placement is **both big models on the DGX, triage on gnuc.** gnuc's role in the pool is
  the cheap/fast/parallel tier (triage, small models), not a planner host. Failover of the
  planner is DGX↔DGX or to another big box, not to gnuc.

## Not yet exercised

- **The triage → revise loop.** Both tasks passed first-pass (qwen3-coder is strong on
  standard algorithms), so the revise leg never fired in-loop. The triage *role* is
  confirmed working in isolation; the convergence behavior of revise needs a task the
  planner genuinely fails first-pass — a future probe (or an induced failure).

## What this de-risks for the Rust build

Backend-pin + structured-output + scoped-edit + worktree-fence + the place-don't-swap
residency story all check out on real hardware. The `newt-scheduler` `BackendPool` can be
built against this measured reality: keep big models warm on the DGX, route triage to
gnuc, and treat model-load (not swap) as the cost to amortize.

## Run it

```
python3 experiments/crew-mvp/crew.py          # fib task
python3 experiments/crew-mvp/crew.py roman    # harder task
```
Prototype only (stdlib Python, hits Ollama `/api/chat` directly) — not production newt.
