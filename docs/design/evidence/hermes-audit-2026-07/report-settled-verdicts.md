# DECISION LEDGER — hermes-agent ↔ newt-agent study (2026-06-10, wf_57b8414c-cdb)

Sources: `docs/design/evidence/hermes-study/*` (4 reader reports, synthesis draft, 3 adversarial verdicts, upstream-offers), `docs/design/context-memory-hermes-learnings.md` (the ACCEPTED plan — "every claim below survived (or was corrected by) the verification pass; where the doc and the draft disagree, the doc is right on purpose"), `progressive-disclosure-memory.md`, `progressive-disclosure-compaction.md`. Hermes was studied @ local checkout of 2026-06-10; newt @ `6b7d780`.

## 1. VERDICTS REACHED (settled 2026-06)

**Headline verdict:** "Hermes's durable wins for newt are four **algorithms**, not its architecture: (1) FTS5-over-everything conversation recall… (2) Structural-prune-before-LLM-summarize compression… (3) The error-path-as-curator memory cap… (4) Prompt-tokens-only accounting." And: "Newt takes the algorithms and refuses the architecture."

- **FTS5 + snippet recall is enough; embeddings/vector store rejected.** Plan: "**No aux-LLM summarization of search hits**… On local models that's slow and expensive; snippets ± context go straight into the tool result." Reaffirmed post-implementation in progressive-disclosure-memory §9: "A vector store / embeddings recall (recall is FTS5 bm25 by design — the hermes study's 'snippet is enough' conclusion stands)."
- **No exact tokenizer — SKIP.** "estimates + safety margins demonstrably suffice; newt's probe ratchet absorbs the error (probe.rs:149-153)." Hermes itself ships `(len+3)//4` + flat image cost; verifier confirmed "no tiktoken import in agent/ or run_agent.py".
- **Newt already ahead — keep as-is:** the empirical probe-and-ratchet "is stronger than hermes's 10-step hand-maintained provider waterfall… it measures instead of catalogues"; frozen-snapshot memory ("hermes's best memory idea, already implemented" — newt's memory.rs "credits hermes by name"); the lean 5-section summary template ("mis-wired, not missing"); lazy record creation.
- **Verifier corrections that are now settled facts:** hermes's template has **13** sections, not 14; hermes does **not** have the "first non-empty wins" wart (that wart is newt's own, memory.rs:245-257); hermes's compressor *is* fed by its discovery system (what it lacks is a persisted per-model tuning store); WS-D's real target is newt's **double-count** ("providers accumulate input+output per turn across history, but prompt tokens already include all prior turns"), not "no real usage at all"; "Phase 9.7 in flight" was refuted — it became "the explicit first deliverable of this effort"; five draft "adoptions" already existed in newt (lazy record creation, MRU-by-updated_at, notes usage header, lean template, real-usage preference).
- **§6 mesh-readiness verdict (maintainer-added, BINDING):** timestamp MRU rejected on both sides — "do *not* port timestamp MRU, and retire newt's own." Ordering = "signed per-writer monotonic tick" + `prev_hash` BLAKE3 content chain; "wall-clock `created_at`… survives **only as a display field — a claim, never the ordering key**." Rationale: agent-mesh-protocol — "Wall-clock time is treated as a claim, never as a coordination primitive." Also: "'Phase 16 `MeshStore`' is a promissory note with no backing code — do not assume it exists."
- **Post-study extension (#319, 2026-06-13):** "a confident summary is worse than a labelled absence, because absence routes the model to retrieval and a summary suppresses that" → memory is "a budgeted, addressable resource the agent navigates on demand — not a blob you summarize and hope."

## 2. ADOPTED (and where it landed in newt)

Per the accepted plan (Phases 17-19) and the later docs' cites of shipped code:

- **SQLite conversation store + FTS5 + query sanitizer** → `newt-core/src/store.rs` (`ConversationStore::search`, `append_turn_full`, `next_tick`, `load_turn` by `(conv, seq)` at store.rs:2045, blake3 `canonical_encoding_v1` at store.rs:969). Honest dep grounds (corrected): whole-file JSON rewrite per turn, tool-event indexing, snippet/ranking quality — NOT lineage or scale.
- **Two-mode recall tool with coaching text** → `newt-core/src/agentic/recall.rs` (`RecallSource` injected trait, `recall_tool_definition`, `execute_recall`; snippets with bm25 marks, "every branch is a tool *result*, never a loop abort", "plain keywords, not boolean/FTS syntax").
- **Structural-prune-before-summarize compression + markers + anti-thrash + redaction** → `newt-core/src/agentic/compress.rs` (`SUMMARY_PREFIX` `[CONTEXT COMPACTION — REFERENCE ONLY]`, `is_compaction_text`, `reread_breadcrumb` (#321), `redact_secrets`, `CompressState` two-sub-10%-reclaims latch). Kept newt's 5-section template + In-Progress slot + verbatim-Active-Task rule + end markers ("newt's primary audience *is* weak local models").
- **Agent-writable memory** → `save_note` tool wired via `merged_tool_definitions(with_recall, with_save_note)` in `newt-core/src/agentic/tools.rs`; NoteStore v2 in `newt-core/src/notes.rs` (frozen `system_prompt_block`, the over-budget-add-returns-entry-list curation cap, `§` delimiter, anti-rot rule); read-only-rounds nudge pattern reused.
- **Continuity surfaces** → #289/#307: `take_compaction_record`/`restore_turns` in `newt-core/src/memory.rs`; `/memory` counters.
- **§6 ordering** → shipped in store.rs: "§6-ordered by `seq`, never re-sorted by `ts_claim`… 'wall-clock is a display claim, not an ordering key'".
- **Post-study, built on the study's substrate:** `memory_fetch` index-then-fetch (memory.rs:771), `MemoryDisclosure::Frozen/Index` flag (config.rs:499), CI-pinned `MEMORY_INDEX_BUDGET = 12` (memory.rs:763). progressive-disclosure-compaction §3: "~80% built… The gap is narrow and specific: **the compaction marker does not carry the `memory_fetch` handles.**"
- **Ephemerality for evals** → `--ephemeral` / `NEWT_CONVERSATION_ID`, "newt-eval runs default ephemeral (the eval gate stays honest — CLAUDE.md)".

## 3. REJECTED + WHY (needs NEW hermes evidence to reopen)

From the plan's "Do NOT copy" (§3) and gap-matrix SKIPs:

1. **Pluggable context-engines / memory-provider plugin ecosystems** — "even hermes added a one-external-provider rule to contain 'tool schema bloat and conflicting memory backends'. Newt's compiled-in `MemoryProvider` trait is the right amount of seam."
2. **Honcho / all hosted memory providers** — "Hosted dependency — hard constraint violation — solving a multi-user gateway problem newt doesn't have."
3. **Background review fork** — "Its economics (byte-exact prompt-cache inheritance, '~26% Sonnet cost cut') are Anthropic-hosted-cache shaped, not Ollama-shaped. The in-band nudge + end-of-session pass gets most of the value with none of the machinery."
4. **10-step context-length waterfall** — "the treadmill of cataloguing every hosted provider; newt's probe measures instead."
5. **Dual-write JSONL+SQLite, prefer-longer-source** — "fossilized migration risk that spawned its own bug class (hermes #860). Migrate once; one truth."
6. **Gateway session keys + handoff state machines** — "Cross-device continuity is Phase 16's (mesh) problem." Likewise the suspended/restart-strike interruption model: "a TUI doesn't restart under live traffic."
7. **13-section template + curator's `max_iterations=9999` fork** — "Both assume frontier models; an unbounded autonomous consolidation agent is the opposite of newt's confined-loop philosophy."
8. **Monolith accretion** — five compression call sites across a 16,408-line run_agent.py; "route all loop-adjacent logic through `newt-core::agentic`."
9. **Timestamp MRU / `last_active = MAX(message ts)`** — rejected by §6 (binding), superseded by signed per-writer tick + chain tip.
10. **@-mention context references** — "SKIP for now: newt's model has `read_file`; TUI users type paths. Revisit on demand." (Softest reject.)
11. **Anthropic prompt-cache breakpoints** — N/A to Ollama/OpenAI-wire; frozen-prefix discipline already in the trait contract.
12. **MEMORY.md/USER.md split** — "single-user; a `user:` prefix convention suffices."
13. **CJK trigram FTS table; aux summary model with fallback chains/cooldowns; 15-retry jittered write convoy** (kept only WAL→DELETE fallback + `busy_timeout`).

## 4. DEFERRED / OPEN (noted, never decided)

- **Skills curator / declare-intent-at-delete (`absorbed_into`) pattern** — "SKIP until agent-authored skills exist; **bookmark** the declare-intent-at-delete pattern."
- **Compression lineage `parent_session_id` + tip-projection** — cut as YAGNI, but explicitly "Add with a real branching feature."
- **`ConversationStore` trait / `JournalProvider` / Phase-16 MeshStore** — trait deferred "until a second store (Phase 16 `MeshStore`) is real"; mesh reconciliation of per-agent merkle logs is "a separate concern… never by trusting a shared clock. Do not design that reconciliation in Phase 17." `agent-mesh-store` flagged as future work.
- **LLM auto-title polish** (heuristic title shipped; "optional LLM polish later"); aux-LLM recaps of recall hits ("Add recaps later only if snippets prove insufficient").
- From progressive-disclosure work: `compaction_archive` retention + secret-retention adversarial review; `verify_chain` participation of archive rows ("conservative default is auxiliary content"); `MEMORY_INDEX_BUDGET` empirical tuning; re-page thrash guard, cross-session paging/GC, knowledge-bank promotion (Step 20.5); paged-vs-summary default flip gated on the #75/#350 eval ("do not ship 'better' on intuition").

## 5. UPSTREAM-OFFERS (newt → hermes, 2026-06-11)

Recommended, in order: (1) **suspicious empty local-model diagnostics** (`e35fa5a`: detect "generated tokens but no visible output", one nudged retry, diagnostic naming `reasoning_content`/`thinking` — "the best first upstream offer"); (2) **first-class `--trace` mode** for support bundles; (3) **empirical local-model capability/tuning cache** (probe.rs `CapabilityEntry` — "what input size has actually worked on this machine?"); (4) **opt-in project-local config overlay** (`.newt/config.toml` deep-merge). Explicitly NOT offered: AGENTS.md loading, HTTP MCP transport, write-file build-check hook, and — decisive for relationship framing — "**Context compression, memory, and session recall.** Hermes is currently ahead of Newt in these areas; Newt's Hermes-study plan mostly points the other direction."

**Gate rule:** every item in §3 carries a constraint-based reason (local-first, no hosted deps, confined loop, weak-model audience, mesh no-wall-clock law). A new audit reopens one only on material hermes change *plus* a change in the newt-side constraint; §4 items are open by design.