# Context-Management Modes — research synthesis (#559 follow-up, task #13)

Synthesis of two sources against newt's actual code, to feed the
`/context manager <name>` selector seam (Step 24.8) and #546.

**Sources**
- *Code as Agent Harness: Toward Executable, Verifiable, and Stateful Agent
  Systems* — [arXiv:2605.18747](https://arxiv.org/abs/2605.18747) (v1,
  2026-05-18; a ~66-page survey/position paper). Reference card:
  [`2605.18747-code-as-agent-harness.md`](./2605.18747-code-as-agent-harness.md)
  (the PDF itself is gitignored). Context-management taxonomy in **§3.2 "Memory
  and Context Engineering"** (pp.21–24, Fig.6, Table 5); shared-substrate
  representations in **§4.3** (pp.43–48).
- `notes.txt` — 5 distilled production patterns (sliding-window+pinning,
  tool-output offloading, dynamic compaction + prompt-cache, scratchpad,
  sub-agent isolation).

## Paper's memory taxonomy (§3.2), by functional role
1. **Working memory** (§3.2.1) — current-trajectory state under budget.
2. **Semantic memory** (§3.2.2) — retrieved *repo evidence*, structure-aligned
   (AST chunking, query rewrite, rerank). *The RAG-for-code line.*
3. **Experiential/episodic** (§3.2.3) — reusable cross-task experience behind a
   **quality write-gate** (quality > scale).
4. **Long-term** (§3.2.4) — governance: when to write/compress/retrieve, dedup,
   anti-drift.
5. **Multi-agent** (§3.2.5) — shared blackboard / state graph.
6. **Compaction & state offloading** (§3.2.6) — provenance-preserving summaries
   **+ retrievable handle** to the full artifact.

Key paper line (p.48): *"context management is the tax of implicit shared
state"* — the tricks are workarounds for lacking a formal queryable substrate.

## What newt ALREADY has (so these are not new work)
- **`standard`** (`compress.rs`) = head-and-tail sliding window + pinning
  (system + verbatim task head, token-budget tail, anchored recent user msg,
  cut aligned past tool pairs) + **LLM-summarize-the-middle** + static-marker
  fallback + **anti-thrash** (`CompressState`) + Phase 24 (#559): warm/keep_alive,
  retry, fallback-model, **chunked/hierarchical** summary.
- **`recall`** — semantic retrieval over *past conversations*.
- **`memory_fetch`** — progressive disclosure of addressed items (note/turn/
  compaction); `compaction:`/`spill:` handles are *recognized but deferred*.
- **`use_skill`** — skill progressive disclosure (card → hydrate).
- Sub-agent isolation / shared blackboard → already owned by **#546** +
  agent-mesh.

## NEW candidate `ContextManager` variants (ranked shortlist)
1. **`semantic`** — structure-aligned repo *evidence* retrieval (AST chunk +
   query-rewrite + rerank). Distinct from `recall` (conversations, not code).
   Highest leverage for a coding agent. Effort: L.
2. **`scratchpad`** — structured `<state>` object (subtasks, open files, vars)
   separate from the log, mutated via `state_set`/`state_get` tools. Cheapest
   durable win for long tasks; attacks lost-in-the-middle. Effort: M–L.
3. **provenance-preserving compaction + tool-output offloading** — upgrade
   `standard` to mint `compaction:<id>`/`spill:<id>` retrievable handles (the
   address is already half-wired in `memory_fetch.rs`) and spill oversized tool
   payloads to a re-readable store. Lossless-on-demand compaction. Effort: M;
   half-built already.
4. **`experiential`** — curated cross-task experience (repair recipes, failure
   cards) behind a quality write-gate; pairs with #546 distributed. Effort: L.
5. **`scheduled`** — per-step *compiled* context view (reset + targeted
   re-inject, L2MAC §4.3.1) instead of a rolling buffer; suits the headless
   wyvern tier. Most experimental. Effort: L.

Each new mode is a new enum variant reporting "not yet available" until built —
same pattern as `progressive`/`distributed` under #546. Issues 1–3 are
independent of #546; issue 4 hooks #546's distributed path.

Relevant files: `agentic/compress.rs` (standard), `agentic/memory_fetch.rs`
(`compaction:`/`spill:` seam), `agentic/recall.rs` (conversation retrieval —
contrast with code-`semantic`).
