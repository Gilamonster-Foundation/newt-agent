# Newt-Agent Context/Memory/Conversation Improvement Plan
*Synthesized from hermes-agent analysis (context engine, memory, sessions) vs. newt-agent current state. 2026-06-10.*

---

## 1. Gap Matrix

Verdicts: **ADOPT** (port the mechanism), **ADAPT** (port the idea, change the mechanism), **HAVE** (newt already does this), **SKIP** (with reason).

### Context-window management

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| Pre-send token guard with compress-up-to-3-passes loop (run_agent.py:12372-12431) | Guard exists — `trim_to_token_budget` before every dispatch (lib.rs:2705-2721, :3692-3706) — but it **discards** | **ADAPT**: keep the guard, replace discard with prune→summarize (WS-B) |
| Real `prompt_tokens` for triggers, completion tokens excluded, rough-estimate fallback (run_agent.py:15311-15331; context_compressor.py:488-491) | chars/4 everywhere (lib.rs:2688-2694; memory.rs:511-514) | **ADOPT** (WS-D) |
| Tool-schema token accounting — "schemas alone can add 20-30K tokens" (model_metadata.py:1805-1825) | Not counted | **ADOPT** (WS-D) |
| Exact tokenizer | Neither codebase has one — hermes ships chars/4 + flat image cost (model_metadata.py:1717-1743) | **SKIP**: hermes proves estimates + safety margins suffice; newt's probe ratchet absorbs the error (probe.rs:149-153) |
| Structural pruning before LLM summary: hash-dedupe, per-tool one-liners, JSON-aware arg shrinking (context_compressor.py:519-685, :178-221) | None — middle replaced by one placeholder line (lib.rs:2666-2671) | **ADOPT** — highest-value single mechanism in hermes (WS-B) |
| Tool-pair sanitization post-compression (context_compressor.py:1118-1176) | Have: `repair_orphaned_tool_calls` two-pass (lib.rs:2791-2858) | **HAVE**; adapt the stub-result-insertion variant so assistant turns survive instead of being stripped |
| LLM summary, 14-section template, Active Task verbatim (context_compressor.py:840-897) | `Summarizing` provider exists but: placeholder without summarizer (memory.rs:834-839), blocks inside `sync_turn` (lib.rs:1290-1305 vs memory.rs:142-144), never called from the trim path (lib.rs:2666-2671) | **ADAPT**: lean ~6-section template, summarize at top-of-loop not in `sync_turn` (WS-B) |
| Iterative summary-of-summary + cross-restart rehydration (context_compressor.py:899-913, :1439-1448) | `prev_summary` exists same-process (memory.rs:800-804) but **wiped on restore** (memory.rs:919) | **ADOPT** rehydration (WS-B) |
| Anti-thrashing: <10% savings twice → stop (context_compressor.py:493-513) | Exists only inside `Summarizing` (memory.rs:778-784) | **ADAPT**: hoist into the shared compression path (WS-B) |
| Last-user-message anchor + END-OF-SUMMARY marker for weak models (context_compressor.py:1245-1290, :1528-1537) | Trim keeps head = system + original task (lib.rs:2655-2678) — implicit, not guaranteed | **ADOPT** — newt runs weak local models; this targets exactly hermes bugs #10896/#11475/#14521 (WS-B) |
| Probe-tier discovery + provenance-gated caching (run_agent.py:14520-14531; model_metadata.py:877-904) | Newt's is **stronger**: empirical ratchet with confidence levels (probe.rs:126-189), parsed-400 limits persisted (probe.rs:156-173) | **HAVE** — keep newt's; do not import the 10-step waterfall |
| Manual `/compress [focus]` + before/after feedback (gateway/run.py:11090-11199; manual_compression_feedback.py:8-49) | None | **ADOPT** (WS-B) |
| @-mention context references with 25%/50% budget gates (context_references.py:132-203) | None | **SKIP for now**: newt's model has `read_file`; TUI users type paths. Revisit if users ask. |
| Anthropic prompt-cache breakpoints (prompt_caching.py:49-79) | N/A — Ollama/OpenAI-wire; frozen prefix discipline already in the trait contract (memory.rs:113-117) | **SKIP** |

### Memory

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| Frozen-snapshot memory in system prompt (memory_tool.py:358-369) | **HAVE** — explicitly documented (memory.rs:113-117, :565-567) | **HAVE** |
| Char-cap self-curation: over-budget add returns full entry list + "Replace or remove first" (memory_tool.py:247-258) | Hard fail, no listing, no guidance (memory.rs:611-617) | **ADOPT** (WS-C) |
| Agent-writable memory tool (run_agent.py:11595-11618) | **None** — human-only `/remember` (lib.rs:1407-1414); zero agent-initiated writes | **ADOPT** — the single biggest memory gap (WS-C) |
| Turn-counted nudge, reset on organic use, modulo hydration (run_agent.py:11055, :12246-12301) | None | **ADAPT**: in-band reminder, no background fork (WS-C) |
| Background review fork with tool whitelist + inherited prompt cache (run_agent.py:4350-4429) | None | **SKIP** — see Do-Not-Copy; replaced by end-of-conversation pass |
| Write-time security scan (invisible unicode, injection regexes) (memory_tool.py:67-104) | None — NOTES.md is injected into the system prompt unscanned | **ADOPT** (WS-C) |
| Usage header `[62% — 1,364/2,200 chars]` (memory_tool.py:390-406) | None | **ADOPT**, trivial (WS-C) |
| Skills curator with absorbed-into reconciliation (curator.py:695-877) | Newt has a skills *index* (lib.rs:2077-2088) but no agent-authored skills to rot | **SKIP** until agent-authored skills exist; bookmark the declare-intent-at-delete pattern |
| External provider plugins, Honcho user modeling (memory_manager.py:204-228; plugins/memory/honcho) | `MemoryProvider` trait exists (memory.rs:103-180) | **SKIP** plugin ecosystem — local-first, opinionated; built-ins only |

### Sessions / persistence / recall

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| SQLite-primary persistence, schema-diff migrations (hermes_state.py:185-251, :463-505) | One pretty-JSON file per conversation (conversation.rs:269-275) | **ADAPT**: rusqlite — see dep argument in WS-A (WS-A) |
| Turn-level tool_calls/tool_name persisted and indexed (hermes_state.py:185-251, :253-306) | Only `(user, assistant)` final text; tool rounds discarded (lib.rs:2268-2285) | **ADOPT** (WS-A) |
| FTS5 dual-index with triggers + query sanitizer (hermes_state.py:253-306, :1797-1847) | No search of any kind (lib.rs:2002-2029) | **ADAPT**: one unicode61 table, port the sanitizer, skip the CJK trigram table (WS-A) |
| `session_search` tool: empty-query browse (zero cost) → FTS → lineage dedupe → aux-LLM recaps (session_search_tool.py:268-538) | None; `prefetch()` is dead surface — no provider implements it (memory.rs:125-127) | **ADAPT**: snippets-first, no aux-LLM recap initially (WS-A) |
| Stable key → rotating id indirection (gateway/session.py:600-665, :1182-1235) | Fresh `new_conversation_id()` every launch (lib.rs:1175) | **ADAPT**: workspace_key → active conversation id (WS-A) |
| Auto-resume + three-state interruption model (session.py:458-492, :856-955; run.py:3229-3295) | No auto-resume; manual `/conversation restore` only (lib.rs:1175; conversation_context_architecture.md:40-42) | **ADAPT** lite: resume-by-default + `is_fresh_reset` distinction (session.py:462-469); skip suspended/restart-strike machinery (WS-A) |
| Compression lineage `parent_session_id` + tip-projection + resume redirect (hermes_state.py:1162-1350, :1621-1684) | None | **ADOPT** `parent_id` column; tip-projection in `/conversation list` (WS-A/WS-B seam) |
| MRU by `last_active = MAX(message ts)` (hermes_state.py:2163-2185) | Prune/list by record timestamps only (conversation.rs:119-127) | **ADOPT** (WS-A) |
| Dual-write JSONL+SQLite, prefer-longer-source guard (session.py:1255-1292, :1341-1358) | Single JSON | **SKIP** dual-write — it's hermes migration scar tissue (#860); do a one-time JSON→SQLite migration instead |
| Cross-platform handoff state machine (run.py:3884-4030) | N/A — no gateway | **SKIP** |
| Jittered write retry + WAL→DELETE NFS fallback (hermes_state.py:128-160, :317-446) | N/A yet | **ADOPT** the WAL fallback only — `~/.newt` can live on NFS (this very workspace does); skip the 15-retry convoy machinery for a single-writer TUI |

---

## 2. Improvement Plan — four workstreams, ranked by leverage

**Sequencing spine:** Phase 9.7 (loop extraction to `newt-core::agentic`, ROADMAP:529-626) is in flight. Anything touching the chat loop lands **after** 9.7 — otherwise every change is written twice into the dual Ollama/OpenAI loops (lib.rs:3571, :4158; limitation 13). WS-A steps 1/3/4 and WS-C steps 1-3 don't touch the loop and can start now. Roadmap numbering: the decision doc's "15.1-15.6" collides with Phase 15 Role Profiles (ROADMAP:896) — register these as **Phase 17 (WS-A), Phase 18 (WS-B), Phase 19 (WS-C/WS-D)** and fix the decision doc's step references in the first PR.

---

### WS-A — Durable conversations + recall (leverage rank 1)

*"A folder is a conversation" + cross-session search. This is the North Star (gilabot#1887) and fixes limitations 2, 3, 6, 9.*

**What to build.** Replace the JSON-per-conversation backend of `ConversationStore` (newt-core/src/conversation.rs) with SQLite at `~/.newt/conversations.db`: `conversations` (id, title, workspace_key, parent_id, persona, started/updated, end_reason) + `turns` (conversation_id, role-pair, user, assistant, **events JSON** — per-round tool name/args-digest/result-summary, real token usage). Add one FTS5 table over `user || assistant || tool_names || tool_args_digest`, trigger-maintained. New `recall` tool + `/recall` command. Auto-resume: on startup, resolve `workspace_key` and reopen the MRU conversation for that key unless `/new` or `[context] resume = false`.

**Stolen from hermes:**
- Trigger-maintained FTS5 over content+tool activity (hermes_state.py:253-306) — tool activity becomes recallable, fixing "the next turn has no record of files read/edited" (lib.rs:1616, :2268-2285).
- Query sanitizer: balanced-phrase protection, dangling-operator trim, auto-quoting of `chat-send`/`P2.2`-style tokens (hermes_state.py:1797-1847) — newt users search for file paths and step numbers constantly.
- Two-mode recall tool: empty query = zero-cost recent-list; keyword = FTS with `snippet()` ±1 context message, full content dropped — "snippet is enough, saves tokens" (session_search_tool.py:268-322; hermes_state.py:2083-2145). Schema text that coaches the model ("USE THIS PROACTIVELY when… 'remember when'", "Use OR between keywords").
- Stable key → rotating id: reset/resume/branch are just re-pointing the mapping; transcripts never destroyed (gateway/session.py:600, :1182-1235).
- `last_active = MAX(turn timestamp) else started_at` MRU (hermes_state.py:2163-2185).
- `is_fresh_reset` vs `was_auto_reset` distinction so a deliberate `/new` doesn't print a confusing "resumed" notice (session.py:462-469).
- Lazy row creation — no DB row until the first successful turn, avoiding ghost empty conversations (run_agent.py:1963, :2548-2568).
- WAL with `journal_mode=DELETE` fallback on "locking protocol"/"disk i/o error", capturing the init error for user-facing messages (hermes_state.py:105-160).

**Deliberately different:**
- **rusqlite (`bundled` + `fts5` features) is the one new dep, and here is the argument:** it is a single vendored C library with no runtime service, no daemon, no network — squarely local-first. The alternatives lose: tantivy is a much larger pure-Rust dep that gives search but not the relational queries we also need (lineage, MRU-by-last-active, tip-projection — hermes does all three in SQL, hermes_state.py:1237-1278); grep-over-JSON gives no ranking, no snippets, no tool-event indexing, and keeps the whole-file-rewrite store the review already flagged. One dep buys search **and** retires the fragile JSON backend. If rusqlite is vetoed, the fallback is keeping JSON + a brute-force scan `recall` (acceptable below ~200 conversations), but FTS is the recommendation.
- **Workspace identity = blake3(git remote + branch), path-UUID fallback** for non-git dirs, exactly as the decision doc proposes (conversation_context_architecture.md:81-103) — fixes the path-fragile UUIDv5 keying (conversation.rs:70-74). Old UUIDv5 directories are read during migration.
- **Keep `ConversationStore` a concrete struct.** The decision doc proposes a `JournalProvider` trait (conversation_context_architecture.md:310-344); defer it. Newt is "opinionated, not extensible," and the trait earns its keep only when MeshStore is real. Swapping internals behind the existing API is a smaller, honest PR.
- **No aux-LLM summarization of search hits** (hermes runs parallel auxiliary-model recaps, session_search_tool.py:198-259). On local models that's slow and expensive; snippets ± context go straight into the tool result. Add recaps later only if snippets prove insufficient.
- **No three-state interruption model.** Hermes needs suspended/resume_pending/policy-reset because a gateway restarts under live traffic (session.py:973-1128; run.py:3229-3295). A TUI does not. One bit of state (MRU + fresh-reset flag) plus a visible "[resumed: <title>, last active <ago>]" banner.

**PR decomposition:**
| # | PR | ~diff |
|---|---|---|
| A1 | `newt-core/src/store.rs`: rusqlite schema + open/migrate (schema-diff reconciliation per hermes_state.py:463-505 — "adding a column to SCHEMA_SQL is all that's needed"), WAL fallback, one-time JSON import; `ConversationStore` rewired; old code path deleted | ~500 |
| A2 | Workspace key v2: blake3(remote+branch) + path fallback + UUIDv5 migration shim (conversation.rs:70-74 replacement); fix decision-doc numbering to Phase 17 | ~200 |
| A3 | FTS5 table + triggers + sanitizer port, unit tests with adversarial queries | ~350 |
| A4 | `/recall` TUI command (browse + search, human-facing) | ~250 |
| A5 | *(after 9.7)* `recall` model tool registered in the agentic loop; coaching schema text | ~250 |
| A6 | *(after 9.7)* Tool-event recording: turn save extended past `(task, reply)` (lib.rs:2268-2285) to include per-round events + real token usage; FTS picks them up via trigger | ~350 |
| A7 | Auto-resume: workspace_key → MRU conversation, `[context] resume = true` default, resume banner, `/new` sets fresh flag (replaces `lib.rs:1175` unconditional fresh id) | ~300 |

**Risks:** bundled SQLite adds compile time and ~1MB binary (accepted, argued above); NFS homes (mitigated by the WAL fallback, A1); auto-resume surprising users (mitigated: banner + `/new` + config off-switch; resume only within same workspace_key).

---

### WS-B — Compression v2: summarize, don't discard (leverage rank 2)

*Fixes limitations 4, 5, 10, 11 and the residual half of #223: the guard exists (lib.rs:3624, :3692-3706) but its only move is amputation.*

**What to build.** A `compress()` pipeline in `newt-core::agentic` (post-9.7 home), called from the existing pre-send guard and mid-loop trim sites, ordered: (1) structural prune, (2) boundary computation with anchors, (3) LLM summary via the injected summarizer, (4) assembly + `repair_orphaned_tool_calls`. `Summarizing` provider's logic migrates into this shared path; the provider's blocking-HTTP-inside-`sync_turn` violation (memory.rs:142-144 vs lib.rs:1290-1305) dies because compression moves to top-of-loop where blocking is legitimate.

**Stolen from hermes:**
- **Phase-1 structural pruning** — the crown jewel: md5-dedupe of identical tool results >200 chars → per-tool one-line summaries (`[terminal] ran 'npm test' -> exit 0, 47 lines`) → JSON-aware tool-arg shrinking that parses, truncates inside the structure, and reserializes so the result stays valid JSON (context_compressor.py:597-621, :224-343, :178-221 — naive byte-slicing caused MiniMax 400 loops, issue #11762). Reclaims most tokens at zero LLM cost — exactly right for local models where every LLM call is slow.
- **Last-user-message anchor** (`_ensure_last_user_message_in_tail`, context_compressor.py:1245-1290) + summary `[CONTEXT COMPACTION — REFERENCE ONLY]` prefix + explicit `--- END OF CONTEXT SUMMARY ---` marker because "weak models read the verbatim Active Task quote as fresh input" (context_compressor.py:37-51, :1528-1537) — newt's primary audience *is* weak local models.
- **Iterative summary + rehydration**: prev-summary fed back as "PREVIOUS SUMMARY / NEW TURNS TO INCORPORATE, continue numbering" (context_compressor.py:899-913), and prefix-scanning the restored transcript to recover summary identity (context_compressor.py:1439-1448) — fixes newt wiping `prev_summary` on restore (memory.rs:919).
- **Anti-thrashing + static fallback marker**: <10% savings twice → stop auto-compressing and tell the user (context_compressor.py:493-513, :1565-1571); on summarizer failure insert "Summary generation was unavailable. N message(s) removed" instead of silent loss (context_compressor.py:1489-1503). Newt already aborts-and-restores on summarizer error (memory.rs:824-831) — keep that for the provider, use the marker for mid-loop.
- **Manual `/compress [focus]`** with before→after message/token feedback and noop detection (gateway/run.py:11090-11199; manual_compression_feedback.py:8-49), including the "fewer messages, MORE tokens" honesty note.

**Deliberately different:**
- **Lean ~6-section summary template** (Active Task verbatim / Completed / In Progress / Key Decisions / Files / Remaining), not hermes's 14 sections (context_compressor.py:840-897). A 7B-32B local model given 14 sections produces 14 paragraphs of mush; fewer slots, stricter instructions.
- **No auxiliary-model slot.** Hermes routes compression to a configurable aux model with fallback chains and cooldown timers (context_compressor.py:766-791, :1056-1070). Newt uses the current backend via the existing `with_summarizer` injection (lib.rs:1263-1314) — one model, one config. Keep only the simplest failure rule: on error, fall back to the static marker.
- **Unify budgets with probe tuning** (limitation 10): provider/compression thresholds read `safe_context`/`max_ok_input` from `model-capabilities.json` (probe.rs:74-108) instead of the disconnected `[memory] context_tokens = 8192` default (memory.rs:462-469). Hermes has no equivalent — this is newt's own probe system finally feeding its own compressor.
- **No engine plugin system** (context_engine.py:9-26). One compressor, compiled in.

**PR decomposition** (all after 9.7; B1 depends on WS-D1):
| # | PR | ~diff |
|---|---|---|
| B1 | Budget unification: compression thresholds + provider `max_tokens` sourced from `CapabilityEntry`; delete the parallel default | ~200 |
| B2 | Structural prune module in newt-core (dedupe, per-tool one-liners, JSON-aware arg shrink) + property tests that output is always valid JSON / valid tool pairing | ~450 |
| B3 | Summarize-don't-discard: trim sites call prune → boundary (head=system+task, last-user anchor) → summarizer → marker assembly; placeholder discard path (lib.rs:2666-2671) retained only as the no-summarizer fallback | ~450 |
| B4 | Continuity: prev-summary chain + restore rehydration (fix memory.rs:919); `Summarizing` provider rebased onto the shared path | ~300 |
| B5 | `/compress [focus]` + feedback line + anti-thrash counters surfaced in `/status` | ~250 |

**Risks:** small-model summaries may be garbage — mitigated because Phase-1 prune does the heavy lifting tokenlessly, anti-thrash disables a useless summarizer, and the static marker bounds the damage; loop-coupling churn if 9.7 slips — B2 (pure functions) can land early, B3+ cannot.

---

### WS-C — Agent-curated memory (leverage rank 3)

*Fixes limitation 7: today the model cannot write memory at all (lib.rs:1407-1414) and `on_session_end` is a no-op everywhere.*

**What to build.** A `save_note` tool (add / replace-by-substring / remove-by-substring) over the existing `NoteStore`, a turn-counted nudge, a write-time security scan, and an optional end-of-conversation extraction pass.

**Stolen from hermes:**
- **The error path as curator**: over-budget add returns the full entry list + "Replace or remove existing entries first" (memory_tool.py:247-258) — the cap *is* the curation policy; no curator daemon needed. Replaces newt's bare hard-fail (memory.rs:611-617).
- **Substring addressing with ambiguity errors** ("Multiple entries matched… Be more specific", memory_tool.py:266-356) and the `§` entry delimiter — upgrades newt's exact-substring dedup (memory.rs:606-608).
- **Usage header** `MEMORY [62% — 1,364/2,200 chars]` in the system-prompt block (memory_tool.py:390-406) so the model budget-plans.
- **Write-time security scan** for invisible unicode and injection/exfiltration patterns, because "memory entries are injected into the system prompt" (memory_tool.py:67-104) — NOTES.md is injected verbatim today (memory.rs:570-698), unscanned.
- **Counter nudge that resets on organic use** (run_agent.py:11055, :12291-12301): only fires after N turns *without* a note write.
- **Prompt-level guidance**: "declarative facts, not instructions to yourself… If a fact will be stale in a week, it does not belong in memory" (prompt_builder.py:150-171), and the tool description's "do NOT save task progress" rule (memory_tool.py:518-535) — task progress belongs to WS-A's recall, not notes.
- **Anti-rot rule** from the skill-review prompt: never store negative capability claims — "these harden into refusals the agent cites against itself for months" (run_agent.py:4049-4143). Goes verbatim into `save_note`'s schema text.
- **File locking** (sidecar lock + atomic rename, memory_tool.py:144-176, :430-459) — newt allows concurrent sessions per workspace.

**Deliberately different:**
- **No background review fork.** Hermes forks a whole agent in a daemon thread with a thread-local tool whitelist and inherited prompt cache (run_agent.py:4350-4429). In a Rust TUI that's a thread + a second API conversation + recursion guards for marginal benefit. Instead: the nudge is a **single appended line on the next user turn** ("System reminder: N turns without a saved note — if you learned a durable fact about this project or user, call save_note; otherwise ignore"), and an **end-of-conversation extraction** runs one tools-disabled completion on `/new`/exit, reusing the exact `final_summary_ollama` no-tools pattern newt already has ("No `tools` key => the model cannot emit tool calls", lib.rs:4024).
- **One store, not two.** Hermes splits MEMORY.md/USER.md (memory_tool.py:55-57). Newt is single-user, single NOTES.md (memory.rs:580); a `user:` prefix convention inside entries is enough.
- Frozen-snapshot semantics are kept as-is — newt already has hermes's best memory idea (memory.rs:113-117, :565-567 vs memory_tool.py:358-369).

**PR decomposition:**
| # | PR | ~diff |
|---|---|---|
| C1 | NoteStore v2: `§` delimiter, substring replace/remove with ambiguity errors, over-budget listing error, usage header, file lock + atomic write | ~400 |
| C2 | Security scan on write (pattern table + invisible-unicode check) + tests with real payloads | ~200 |
| C3 | Replace `MemoryManager::add_note`'s string-name routing hack (memory.rs:309-314) with a proper trait method; `/remember` rides the new path | ~150 |
| C4 | *(after 9.7)* `save_note` tool registration + schema text (guidance + anti-rot rules) + nudge counter in the loop | ~350 |
| C5 | *(after 9.7, optional)* End-of-conversation extraction pass, config-gated | ~250 |

**Risks:** model writes junk → cap + scan + a visible "note saved: …" line in the TUI so the human sees every write; nudge annoys the model into noise-writes → default interval 10 like hermes (run_agent.py:1997), reset-on-use means quiet sessions only.

---

### WS-D — Token accounting truth (leverage rank 4, but sequenced first among loop changes)

*Small enabler for WS-B; fixes limitation 1's worst edges without buying a tokenizer.*

**What to build.** In the (post-9.7) loop: prefer the backend's reported `input_tokens` from the last response as the current context size, fall back to chars/4 only when absent; include tool schemas in the pre-send estimate; ceiling-divide.

**Stolen from hermes:** prompt-tokens-only triggering — completion/reasoning tokens don't occupy the window, and thinking models inflate them (run_agent.py:15316-15323, hermes #12026; directly relevant to newt's DGX/thinking-model work); stale-zero fallback to rough estimate (run_agent.py:15311-15331); tool-schema accounting (model_metadata.py:1805-1825); `(len+3)/4` ceiling (model_metadata.py:1717-1726).

**Deliberately different:** no tokenizer dep (see gap matrix row 4); no image costing until newt is multimodal; feed the result into the *existing* `send_budget` guard (lib.rs:3624) rather than a new trigger system.

**PRs:** D1 — `estimate_request_tokens(messages, tools)` + real-usage-preferred plumbing (~250); D2 — `TokenBudget`/`Summarizing` consume real usage on restore instead of always re-estimating (memory.rs:543, :916) (~150).

**Sequencing summary:** D1 → B1 → B2..B5; A1-A4 parallel anytime; A5-A7, C4-C5 after 9.7. Total: ~16 PRs, all within the 100-500 line discipline, each independently green under the 80% coverage floor.

---

## 3. Do NOT copy

1. **The pluggable context-engine / memory-provider plugin ecosystems** (context_engine.py:9-26; plugins/memory/, memory_manager.py:204-228). Hermes needs runtime-loadable engines because it serves many deployment shapes; newt is "opinionated, not extensible." Even hermes had to add a one-provider-max rule to contain "tool schema bloat and conflicting memory backends" (memory_manager.py:204-228). Newt already has the right seam — the compiled-in `MemoryProvider` trait — and should also *not* replicate hermes's "first non-empty wins" composition wart, which newt already shares (memory.rs:245-257).
2. **Honcho and all hosted memory providers** (plugins/memory/honcho/__init__.py:239-247). Hosted-service dependency, hard constraint violation; and its payload — per-peer user representations with dialectic LLM supplements (honcho:547-831) — solves a multi-user gateway problem newt doesn't have.
3. **The background review fork** (run_agent.py:4350-4429). Its justifying economics (byte-exact prefix-cache inheritance, ~26% Sonnet cost cut) apply to hosted Anthropic caching, not Ollama. The machinery it drags in — daemon threads, thread-local tool whitelists, recursion guards via zeroed nudge intervals (run_agent.py:4367) — is exactly the complexity newt exists to avoid. WS-C's in-band nudge + end-of-session pass gets ~80% of the value.
4. **The 10-step context-length resolution waterfall** (model_metadata.py:1428-1714) with its hand-maintained provider tables, fuzzy-match defaults, and cache-invalidation shims for specific providers' underreports (model_metadata.py:1493-1529). That's the maintenance treadmill of supporting every hosted provider. Newt's empirical probe-and-ratchet (probe.rs:126-189) is *better* for local backends: it measures instead of cataloguing. Keep only the principle newt already implements — persist parsed limits, never guesses (probe.rs:156-173 ≈ run_agent.py:14520-14531).
5. **Dual-write JSONL+SQLite with prefer-longer-source reads** (session.py:1255-1292, :1314-1360). This is fossilized migration risk management — it spawned its own bug class (#860 duplicate writes) and a permanent "which source is truth?" tax. Newt should migrate once in WS-A1 and have one truth.
6. **The cross-platform session-key format and handoff state machine** (gateway/session.py:600-665; run.py:3884-4030). Platform/chat_type/thread/user keying, suspended/restart-strike escalation (run.py:3005), home-channel resolution — all gateway-shaped. If newt ever needs cross-device continuity it goes through mesh (Phase 16, ROADMAP:958), not a port of this.
7. **The 14-section summary template and the curator's `max_iterations=9999` fork** (context_compressor.py:840-897; curator.py:1691-1710). Both assume frontier-class models with large budgets. Small local models given 14 sections fill 14 sections badly; an unbounded-iteration autonomous consolidation agent is the opposite of newt's confined-loop philosophy (max_tool_rounds cap, lib.rs:3955-3968).
8. **Monolith accretion.** `run_agent.py` carries compression triggers at four call sites across 16k+ lines (12372, 15302, 14220, 10640) and a 1,583-line compressor file. Newt's discipline — small modules in newt-core with inline tests (memory.rs's ~45 tests, probe.rs's ~40) and the 9.7 extraction — is the asset that makes every workstream above reviewable. The plan deliberately routes all loop-adjacent logic through `newt-core::agentic` for that reason.

**Bottom line:** hermes's durable wins for newt are (a) FTS5-over-everything recall with lineage, (b) structural-prune-before-summarize, (c) the error-path-as-curator memory cap, and (d) prompt-tokens-only accounting. Its plugin ecosystems, hosted providers, and fork-based background agents are the costs of being a multi-platform gateway — newt should take the algorithms and refuse the architecture.