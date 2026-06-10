# Design: context, memory & conversation improvements — learnings from hermes-agent

**Status:** accepted plan, registered as ROADMAP Phases 17-19.
**Provenance:** 8-agent study (4 deep-readers → synthesis → 3-lens adversarial
verification) over hermes-agent (NousResearch, MIT) and newt-agent @ `6b7d780`,
2026-06-10. Raw reports and verdicts: [`evidence/hermes-study/`](evidence/hermes-study/).
This document is the verified synthesis — every claim below survived (or was
corrected by) the verification pass; the draft and the verdicts are committed
for audit.

Hermes file:line cites refer to the hermes-agent checkout of 2026-06-10;
newt cites refer to `6b7d780`. `lib.rs` unqualified = `newt-tui/src/lib.rs`.

---

## TL;DR

Hermes's durable wins for newt are four **algorithms**, not its architecture:

1. **FTS5-over-everything conversation recall** with a query sanitizer and a
   two-mode (browse/search) tool (`hermes_state.py:253-306, :1797-1847`;
   `tools/session_search_tool.py:268-538`).
2. **Structural-prune-before-LLM-summarize compression** — hash-dedupe of
   repeated tool results, per-tool one-liners, JSON-aware arg shrinking —
   reclaiming most tokens at zero LLM cost (`context_compressor.py:519-685`).
3. **The error-path-as-curator memory cap** — an over-budget memory add
   returns the full entry list plus "replace or remove first", making the cap
   itself the curation policy (`memory_tool.py:247-258`).
4. **Prompt-tokens-only accounting** — completion/reasoning tokens don't
   occupy the window; thinking models inflate them (`run_agent.py:15311-15331`).

Its plugin ecosystems, hosted memory providers (Honcho), background review
forks, and gateway state machines are the costs of being a multi-platform
gateway. Newt takes the algorithms and refuses the architecture (§ Do NOT copy).

**Where newt is already ahead** (verified, keep as-is):

- The empirical **probe-and-ratchet** context tuning (`model-capabilities.json`,
  confidence levels, persisted parsed-400 limits — `newt-tui/src/probe.rs:126-189,
  :156-173`) is stronger than hermes's 10-step hand-maintained provider
  waterfall (`model_metadata.py:1428-1714`): it measures instead of catalogues.
- **Frozen-snapshot memory semantics** (`newt-core/src/memory.rs:113-117,
  :562-571` — which credits hermes by name) are hermes's best memory idea,
  already implemented.
- A **lean 5-section summary template** already exists in the `Summarizing`
  provider (`memory.rs:816-821`) — it is mis-wired, not missing.
- **Lazy record creation** (no conversation record until the first successful
  turn — `conversation.rs:82-89`, `lib.rs:2275-2284`) and **MRU-by-activity**
  (`updated_at` bumped per turn — `conversation.rs:122, :278-283`) already
  exist; hermes ports of either would be redundant.

---

## 1. Gap matrix

Verdicts: **ADOPT** (port the mechanism) / **ADAPT** (port the idea, change
the mechanism) / **HAVE** (newt already does this) / **SKIP** (with reason).
Phase tags refer to § 2.

### Context-window management

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| Pre-send token guard with compress-up-to-3-passes loop (run_agent.py:12372-12431) | Guard exists — `trim_to_token_budget` before every dispatch (lib.rs:2705-2721, :3692-3706) — but its only move is **discarding** the middle for a placeholder line (lib.rs:2666-2671) | **ADAPT**: keep the guard, replace discard with prune→summarize (P18) |
| Prompt-tokens-only triggers; completion/reasoning excluded (thinking models, hermes #12026); stale-zero fallback to estimate (run_agent.py:15311-15331) | Providers already prefer real usage (memory.rs:507-514, :888-892) — but sum **input+output per turn, accumulated**, so prompt tokens (which already include all history) are double-counted as history grows; the loop estimate is chars/4 (lib.rs:2688-2694) | **ADAPT**: fix the double-count; prompt-tokens-only (P18) |
| Tool-schema token accounting — "schemas alone can add 20-30K tokens" (model_metadata.py:1805-1825) | Not counted: `estimate_tokens` sums messages only (lib.rs:2688-2694) while the body separately carries `tools` (lib.rs:3712-3727) | **ADOPT** (P18) |
| Exact tokenizer | Neither codebase has one — hermes ships `(len+3)//4` + flat image cost (model_metadata.py:1717-1743) | **SKIP**: estimates + safety margins demonstrably suffice; newt's probe ratchet absorbs the error (probe.rs:149-153) |
| Structural pruning before LLM summary: md5-dedupe of >200-char tool results, per-tool one-liners (`[terminal] ran 'npm test' -> exit 0, 47 lines`), JSON-aware arg shrinking that parses/truncates/reserializes (naive byte-slicing caused MiniMax 400-loops, hermes #11762) (context_compressor.py:519-685, :178-221, :224-343) | None | **ADOPT** — the highest-value single mechanism in hermes (P18) |
| Tool-pair boundary **prevention** (`_align_boundary_forward/backward`, context_compressor.py:1178+) then post-hoc sanitization with stub-result insertion (:1118-1176) | Post-repair only: `repair_orphaned_tool_calls` strips `tool_calls` (assistant text survives with a stub) and drops orphaned results (lib.rs:2791-2858) | **HAVE** the repair; **ADOPT** boundary alignment + the stub-result variant that preserves call/result *pairing* (P18) |
| LLM summary with Active-Task-verbatim rule (context_compressor.py:840-846); 13-section template (:840-893) | `Summarizing` provider exists with a 5-section template (memory.rs:816-821) but: placeholder text when no summarizer is injected (memory.rs:833-839), blocking HTTP inside `sync_turn` against the trait's contract (lib.rs:1290-1305 vs memory.rs:140-144), and never called from the trim path (lib.rs:2655-2678) | **ADAPT**: keep newt's lean template (+ In-Progress slot + verbatim-Active-Task rule); relocate summarization to top-of-loop (P18). The 13-section template itself is a do-not-copy |
| Iterative summary-of-summary ("PREVIOUS SUMMARY / NEW TURNS TO INCORPORATE, continue numbering") + cross-restart rehydration by prefix-scan (context_compressor.py:899-913, :1439-1448) | `prev_summary` chains same-process (memory.rs:799-804) but is **wiped on restore** (memory.rs:919) | **ADOPT** rehydration (P18) |
| Anti-thrashing: <10% savings twice → stop auto-compressing, tell the user (context_compressor.py:493-513) | Exists only inside `Summarizing` (memory.rs:774-785); nothing on the trim path | **ADAPT**: hoist into the shared compression path (P18) |
| Last-user-message tail anchor + `[CONTEXT COMPACTION — REFERENCE ONLY]` prefix + `--- END OF CONTEXT SUMMARY ---` marker, added for weak models (hermes bugs #10896/#11475/#14521) (context_compressor.py:1245-1290, :37-51, :1527-1537) | Trim keeps head = system + original task (callers pass `head: 2`) — implicit, no anchors, no markers | **ADOPT** — newt's primary audience *is* weak local models (P18) |
| Token-budgeted tail protection (`protect_tail_tokens`, `_find_tail_cut_by_tokens` — context_compressor.py:519-595, :1292-1300) | Tail is message-count-shaped | **ADOPT** — a count-based tail with a few huge tool results defeats the pipeline (P18) |
| Secret redaction on all summarizer input + "NEVER include API keys… [REDACTED]" preamble (context_compressor.py:725, :746, :833-836) | No redaction anywhere on the summary path | **ADOPT** — summaries persist and re-inject for the life of a conversation (P18) |
| Probe-tier discovery + provenance-gated caching ("only persist limits parsed from the provider's error message") (run_agent.py:14520-14531; model_metadata.py:877-910) | Newt's is **stronger**: empirical ratchet with confidence levels (probe.rs:126-189), parsed-400 limits persisted (probe.rs:156-173) | **HAVE** — keep newt's; do not import the waterfall |
| Manual `/compress [focus]` with before/after feedback incl. the "fewer messages can still raise this estimate" honesty note (gateway/run.py:11088-11204; manual_compression_feedback.py:8-49) | None (no `/compress`; nearest surface is `/memory`, lib.rs:1393-1405) | **ADOPT** (P18) |
| @-mention context references with 25%/50% budget gates (context_references.py:132-203) | None | **SKIP for now**: newt's model has `read_file`; TUI users type paths. Revisit on demand |
| Anthropic prompt-cache breakpoints (prompt_caching.py:49-79) | N/A — Ollama/OpenAI-wire; frozen-prefix discipline already in the trait contract (memory.rs:113-117) | **SKIP** |

### Memory

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| Frozen-snapshot memory in the system prompt (memory_tool.py:11-14, :358-369) | **HAVE** (memory.rs:113-117, :562-571) | **HAVE** |
| Char-cap self-curation: over-budget add returns the full entry list + "Replace or remove existing entries first" (memory_tool.py:247-258) | Bare hard fail with counts, no listing, no guidance (memory.rs:611-617) | **ADOPT** (P19) |
| Agent-writable memory tool (run_agent.py:11594-11618) | **None** — `/remember` is human-only (lib.rs:1407-1414); the model's tool set has no memory tool (lib.rs:2473-2569); `on_session_end` is never overridden *and never called* (memory.rs:167, :293-297) | **ADOPT** — the single biggest memory gap (P19) |
| Turn-counted nudge, reset on organic use, default interval 10, modulo hydration (run_agent.py:1997, :11055-11057, :12246-12301) | None for memory — but the loop already has the exact pattern: the read-only-rounds nudge with reset-on-use (lib.rs:3646-3664) | **ADAPT**: in-band reminder reusing newt's own nudge pattern; no background fork (P19) |
| Background review fork: daemon thread, thread-local tool whitelist, inherited byte-exact prompt cache ("~26% Sonnet cost cut") (run_agent.py:4350-4429, :4502) | None | **SKIP** — see Do-Not-Copy #3; replaced by an end-of-conversation pass |
| Write-time security scan: invisible-unicode set + injection/exfiltration regexes, because "memory entries are injected into the system prompt" (memory_tool.py:67-104) | None — NOTES.md is injected verbatim (memory.rs:600-622, :672-682) | **ADOPT** (P19) |
| Usage header `[62% — 1,364/2,200 chars]` (memory_tool.py:390-406) | **Mostly HAVE**: block header already renders `## Agent Notes ({used}/{limit})` (memory.rs:676-681) and `/memory` prints percentage (lib.rs:1395-1403) | **HAVE**, one-line format tweak rides P19.1 |
| Substring addressing with ambiguity errors + `§` entry delimiter (memory_tool.py:59, :266-356) | Exact-substring dedup only (memory.rs:606-608) | **ADOPT** (P19) |
| Anti-rot rule: never store negative capability claims — "harden into refusals the agent cites against itself for months" (run_agent.py:4119-4128) | N/A (no agent writes yet) | **ADOPT** verbatim into the tool schema text (P19) |
| File locking + atomic write (memory_tool.py:144-176, :430-459) | Plain `fs::write` (memory.rs:640-646) — though the write-then-rename idiom already exists in `ConversationStore::save_record` (conversation.rs:200-215) | **ADOPT**, copying newt's own idiom (P19) |
| MEMORY.md/USER.md split (memory_tool.py:5-9, :179-183) | Single NOTES.md (path: memory.rs:591-597) | **SKIP**: single-user; a `user:` prefix convention suffices |
| Skills curator with `absorbed_into` reconciliation (curator.py:695-720) | Skills *index* only (lib.rs:2077-2088); no agent-authored skills to rot | **SKIP** until agent-authored skills exist; bookmark the declare-intent-at-delete pattern |
| External memory-provider plugins + Honcho user modeling (memory_manager.py:204-228; plugins/memory/honcho) | Compiled-in `MemoryProvider` trait (memory.rs:103-180) | **SKIP** — local-first, opinionated; built-ins only |

### Sessions / persistence / recall

| Hermes capability | Newt current state | Verdict |
|---|---|---|
| SQLite-primary persistence; schema-diff migrations ("adding a column to SCHEMA_SQL is all that's needed") (hermes_state.py:185-251, :463-505) | One pretty-JSON file per conversation, **whole file rewritten every turn** (conversation.rs:119-127, :200-215, :269-271) | **ADAPT**: rusqlite — argument in P17 (P17) |
| Turn-level `tool_calls`/`tool_name` persisted and indexed (hermes_state.py:185-251) | Only final `(user, assistant)` text; tool rounds and usage discarded (conversation.rs:8-12; lib.rs:2268-2285; loop returns only `(reply, streamed, usage, hallucinations)`, lib.rs:3574) | **ADOPT** (P17) |
| FTS5 trigger-maintained index + query sanitizer (balanced phrases, dangling-operator trim, auto-quoting of `chat-send`/`P2.2`-style tokens) (hermes_state.py:253-306, :1797-1847) | No search of any kind (`/conversation` = list/show/restore/rename/delete, lib.rs:1993-2030); `prefetch()` is dead surface — no provider implements it, no call site (memory.rs:125-127, :227-239) | **ADAPT**: one unicode61 table + the sanitizer; skip the CJK trigram table (P17) |
| Two-mode `session_search` tool: empty query = zero-cost recent list → FTS with `snippet()` ±1 context, full content dropped ("snippet is enough, saves tokens") → coaching schema text ("USE THIS PROACTIVELY when… 'remember when'") (session_search_tool.py:268-538; hermes_state.py:2083-2147) | None | **ADAPT**: snippets-first; **no aux-LLM recaps** (slow/expensive on local models) (P17) |
| Auto-title from the first exchange, async, "never adds latency", with first-N-chars preview fallback (title_generator.py:1-30; hermes_state.py:1260-1265) | Titles exist as a column but nothing produces one | **ADAPT**: cheap heuristic title (first user line), optional LLM polish later (P17) |
| Stable key → rotating id indirection; reset/resume/branch re-point the mapping, transcripts never destroyed (gateway/session.py:600-665, :918, :1182-1235) | Fresh `new_conversation_id()` every launch (lib.rs:1175); `/new` mints another (lib.rs:2220-2233) | **ADAPT**: workspace_key → active-conversation mapping (P17) |
| Auto-resume + `is_fresh_reset` vs `was_auto_reset` (so a deliberate `/new` doesn't print a confusing "resumed" notice, hermes #6508) (session.py:458-492) | No auto-resume; manual `/conversation restore` only | **ADAPT** lite: resume MRU by default + banner + fresh-flag; **skip** the suspended/restart-strike machinery — a TUI doesn't restart under live traffic (P17) |
| MRU `last_active = MAX(message ts)` (hermes_state.py:2163-2185) | **Mostly HAVE**: `updated_at` bumps per turn and drives ordering (conversation.rs:122, :139-141, :278-283). Real defects: no per-turn timestamps, and `rename` also bumps `updated_at` (conversation.rs:154-159) polluting MRU | **HAVE**; 3-line fix + per-turn timestamps ride P17 |
| Lazy row creation (deferred to the start of the first conversation run, retry-on-failure) (run_agent.py:1963, :12126, :2548-2568) | **HAVE** — record created only on the first successful turn (conversation.rs:82-89; lib.rs:2275-2284) | **HAVE** — carry behavior through the P17 rewrite |
| Compression lineage `parent_session_id` + tip-projection + resume redirect (hermes_state.py:1162-1350, :1621-1684) | No lineage | **SKIP** (changed from draft): nothing in newt forks conversations — compression mutates provider-internal history. YAGNI per the roadmap's own standard (ROADMAP Step 9.7's trait deferral). Add with a real branching feature |
| Dual-write JSONL+SQLite, prefer-longer-source reads (session.py:1255-1292, :1314-1360) | Single JSON | **SKIP** — hermes's own migration scar tissue (their #860); one-time migration instead |
| Cross-platform handoff state machine (gateway/run.py:3824-3935) | N/A — no gateway | **SKIP**; cross-device continuity goes through mesh (Phase 16), not a port |
| WAL with `journal_mode=DELETE` fallback on "locking protocol"/"disk i/o error"; jittered 15-retry write convoy (hermes_state.py:54-57, :105-160, :317-446) | N/A yet | **ADOPT** the WAL fallback (`~/.newt` can live on NFS — this development workspace does) + a `busy_timeout` pragma (two newts can share the db, conversation.rs:296-299); **skip** the retry convoy |

---

## 2. The plan — ROADMAP Phases 17-19

**Sequencing gate.** Step 9.7 (extract the agentic loop into
`newt-core::agentic`, ROADMAP:552-581) is **specced, not started** — no
`agentic/` module exists and the loop lives twice (`chat_complete`
lib.rs:3571, `openai_chat_complete` lib.rs:4158). Every loop-touching PR
below (marked †) lands after it; writing them pre-9.7 would mean writing them
twice. **Landing 9.7 is the explicit first deliverable of this effort.**
Pre-9.7 runway (can start immediately, in parallel): 17.1-17.4, 17.7, 18.3,
19.1-19.2, plus the benchmark baseline (see
[`docs/testing/context-memory-benchmark.md`](../testing/context-memory-benchmark.md)).

Phase numbering: 17 = conversations+recall, 18 = token truth + compression
(one phase — the token work exists only to enable the compression work),
19 = agent-curated memory. The earlier decision doc's "15.1-15.6" step
numbers collide with ROADMAP Phase 15 (Role Profiles) and are superseded
(§ 4).

### Phase 17 — Durable conversations + recall (leverage rank 1)

*The "a folder is a conversation" North Star (gilabot#1887 / the
conversation-context decision doc) plus cross-session recall.*

**What:** replace `ConversationStore`'s JSON backend with SQLite at
`~/.newt/conversations.db` — `conversations` (id, title, workspace_key,
persona, started/updated, end_reason) + `turns` (conversation_id, user,
assistant, per-turn timestamp, **events JSON** — per-round tool
name/args-digest/result-summary — and **token usage**, a day-one column
because 18.x consumes it). One trigger-maintained FTS5 table over
`user || assistant || tool_names || tool_args_digest`. A `recall` tool +
`/recall` command. Auto-resume by workspace key.

**The dependency argument (rusqlite, `bundled` + `fts5` — the one new dep).**
Honest grounds: (a) the current store rewrites the entire pretty-printed JSON
file on every turn (conversation.rs:119-127); (b) tool-event recall needs an
index, not a grep; (c) ranking + `snippet()` quality is the difference between
a usable `recall` tool and noise. *Not* grounds (corrected from draft):
lineage (cut — YAGNI) and scale (newt prunes to 100 conversations/workspace
by default — `config.rs:158-163`). **Retention decision rides 17.1a:** durable
recall is incompatible with silently pruning to 100; raise the default cap and
make pruning size-based, argued in that PR's body. Fallback if rusqlite is
vetoed: keep JSON + brute-force scan (acceptable at the current cap, defeats
the point). blake3 also becomes a new *direct* dep of newt-core in 17.2
(already in-tree via newt-identity) — both get explicit PR-body justification
per the acceptance contract.

**Workspace identity** = `blake3(git remote + branch)` with path-UUID fallback
for non-git dirs, exactly as the decision doc proposes (its §"workspace
identity"); fixes path-fragile UUIDv5 keying (conversation.rs:70-74). The
per-workspace namespace dir (conversation.rs:273-275) already exists — what's
new is the derivation and the key→active-conversation mapping.

| PR | What | ~diff |
|---|---|---|
| 17.1a | `newt-core/src/store.rs`: rusqlite schema (incl. tokens column) + schema-diff migration + WAL→DELETE fallback + `busy_timeout`; `ConversationStore` API preserved, backend swapped; retention policy change argued here | ~600 |
| 17.1b | One-time JSON import; old write path deleted; decision-doc storage section superseded (§ 4) | ~400 |
| 17.2 | Workspace key v2: blake3(remote+branch) + path fallback + UUIDv5-dir migration | ~200 |
| 17.3 | FTS5 table + triggers + ported query sanitizer; adversarial-query tests (paths, `P2.2`, operators) | ~350 |
| 17.4 | `/recall` command (browse + search) + heuristic auto-title + the rename-doesn't-bump-MRU fix | ~300 |
| 17.5† | `recall` model tool with coaching schema text | ~250 |
| 17.6† | Tool-event + token-usage recording: extend the turn save past `(task, reply)` (lib.rs:2268-2285); FTS picks events up via trigger | ~350 |
| 17.7 | Auto-resume: workspace_key → MRU, `[context] resume = true`, resume banner, `/new` fresh-flag, **`--ephemeral` + `NEWT_CONVERSATION_ID` override; newt-eval runs default ephemeral** (the eval gate stays honest — CLAUDE.md) | ~350 |

**Risks:** bundled SQLite adds compile time + ~1MB binary (accepted, argued
above); NFS homes (WAL fallback); auto-resume surprising users (banner +
`/new` + config off-switch; resume only within the same workspace key).

### Phase 18 — Token truth + compression v2 (leverage rank 2)

*Fixes the residual half of #223: the pre-send guard exists (lib.rs:3624,
:3692-3706) but its only move is amputation. All PRs except 18.3 are †.*

**What:** a `compress()` pipeline in `newt-core::agentic` (post-9.7 home),
called from the existing guard and mid-loop trim sites, ordered: (1)
structural prune → (2) boundary computation (token-budgeted tail, head =
system + original task, last-user anchor, cuts aligned past tool pairs) →
(3) LLM summary via the injected summarizer, with secret redaction on input →
(4) assembly with `[REFERENCE ONLY]`/end-of-summary markers +
`repair_orphaned_tool_calls`. The `Summarizing` provider's logic migrates
into this shared path; its blocking-HTTP-inside-`sync_turn` contract
violation (lib.rs:1290-1305 vs memory.rs:140-144) dies because compression
moves to top-of-loop where blocking is legitimate.

**Token accounting first** (it gates the rest): prefer the backend's reported
prompt tokens for "how full am I" — fixing the **double-count** (providers
accumulate input+output per turn across history, but prompt tokens already
include all prior turns — memory.rs:507-514, :888-892); count tool schemas in
the pre-send estimate; `(len+3)/4` ceiling on the fallback. No tokenizer dep —
hermes proves margins absorb estimate error, and newt's probe ratchet already
provides the margins.

**Budget unification.** Compression thresholds and provider budgets source
from `model-capabilities.json` (`safe_context`/`max_ok_input`) instead of the
disconnected `[memory] context_tokens` default of 8,192 (lib.rs:1260-1264;
memory.rs:462-469). Crate-boundary note (corrected from draft):
`CapabilityEntry` lives in newt-tui (probe.rs:73-108) and newt-core cannot
depend on newt-tui — so the TUI **injects resolved budget values at provider
construction**, mirroring the existing `with_summarizer` injection
(lib.rs:1263-1314). Hermes parallel: its compressor *is* fed by discovery
(`compressor.update_model`, run_agent.py:14512-14519); what hermes lacks is
newt's persisted per-model tuning store.

| PR | What | ~diff |
|---|---|---|
| 18.1† | Token truth: prompt-tokens-preferred accounting, double-count fix, tool-schema estimation, ceiling-divide — feeding the *existing* `send_budget` guard | ~250 |
| 18.2† | Budget unification via construction-time injection; delete the parallel 8,192 default | ~200 |
| 18.3 | Structural prune module in newt-core (**pure functions, lands pre-9.7**): dedupe, per-tool one-liners, JSON-aware arg shrink; property tests: output always valid JSON, tool pairing always intact | ~450 |
| 18.4† | Summarize-don't-discard: guard + trim sites call prune → boundary → redacted summary → marker assembly; static "Summary generation was unavailable; N messages removed" fallback on summarizer failure; placeholder-discard retained only as the no-summarizer path | ~700 (split assembly/boundary if it runs hot) |
| 18.5† | Continuity: prev-summary chain rehydrated on restore (fix memory.rs:919); `Summarizing` provider rebased onto the shared path; **tokens restored from the 17.1a column instead of chars/4 re-estimation** (fix memory.rs:543, :916 — depends on 17.6) | ~300 |
| 18.6† | `/compress [focus]` + before/after honesty feedback + anti-thrash counters surfaced in `/memory` (there is no `/status`) | ~250 |

**Deliberately different from hermes:** keep newt's 5-section template
(+ In-Progress slot + the verbatim-Active-Task rule + markers) — small local
models given hermes's 13 sections produce 13 paragraphs of mush; no auxiliary
summary model with fallback chains and cooldowns (one model, one config); no
pluggable engine.

**Risks:** small-model summaries may be garbage — bounded because the prune
phase does the heavy lifting tokenlessly, anti-thrash disables a useless
summarizer, and the static marker caps the damage. If 9.7 slips, 18.3 still
lands; 18.4+ cannot.

### Phase 19 — Agent-curated memory (leverage rank 3)

*Today the model cannot write memory at all, and `on_session_end` is dead
surface.*

**What:** a `save_note` tool (add / replace-by-substring /
remove-by-substring) over the existing `NoteStore`, a turn-counted in-band
nudge, a write-time security scan, an optional end-of-conversation
extraction pass.

| PR | What | ~diff |
|---|---|---|
| 19.1 | NoteStore v2: `§` delimiter, substring replace/remove with ambiguity errors, **over-budget add returns the full entry list** (the cap is the curator), % in the existing header, atomic write (copy conversation.rs:200-215's idiom) + file lock; includes the ~20-line `MemoryManager::add_note` routing cleanup (the trait method already exists — memory.rs:177-179; only the `name() == "note_store"` lookup at memory.rs:307-316 is the hack) | ~450 |
| 19.2 | Write-time security scan (invisible unicode + injection patterns) with real-payload tests — NOTES.md is system-prompt-injected verbatim today | ~200 |
| 19.3† | `save_note` tool + schema text (declarative-facts guidance, "do NOT save task progress — that's `recall`'s job", the anti-rot rule verbatim) + turn-counted nudge **reusing the read-only-rounds pattern** (lib.rs:3646-3664), default interval 10, reset on organic use | ~350 |
| 19.4† | (optional, config-gated) End-of-conversation extraction: one tools-disabled completion on `/new`/exit, reusing the cap-exit no-tools pattern ("No `tools` key => the model cannot emit tool calls", lib.rs:4024) | ~250 |

**Deliberately different:** no background review fork (§ Do-Not-Copy #3) —
the nudge is one appended line on the next user turn; one NOTES.md, not a
MEMORY/USER split. **Risks:** junk writes → cap + scan + a visible
"note saved:" line in the TUI; nudge noise → reset-on-use means only quiet
sessions ever see it.

**Totals: ~20 PRs** (8 + 6 + 4, plus 9.7 itself and the benchmark baseline),
each within the roadmap's per-step diff norms, each independently green under
the 80% floor.

---

## 3. Do NOT copy

1. **Pluggable context-engines / memory-provider plugins** (context_engine.py:9-26;
   memory_manager.py:204-228). Hermes needs runtime-loadable engines for many
   deployment shapes; even hermes added a one-external-provider rule to
   contain "tool schema bloat and conflicting memory backends"
   (memory_manager.py:8). Newt's compiled-in `MemoryProvider` trait is the
   right amount of seam.
2. **Honcho and all hosted memory providers** (plugins/memory/honcho). Hosted
   dependency — hard constraint violation — solving a multi-user gateway
   problem newt doesn't have.
3. **The background review fork** (run_agent.py:4350-4429): daemon thread,
   thread-local tool whitelist, recursion guards. Its economics (byte-exact
   prompt-cache inheritance, "~26% Sonnet cost cut") are Anthropic-hosted-cache
   shaped, not Ollama-shaped. The in-band nudge + end-of-session pass gets
   most of the value with none of the machinery.
4. **The 10-step context-length waterfall** (model_metadata.py:1428-1714) with
   hand-maintained provider tables and per-provider cache-invalidation shims.
   That's the treadmill of cataloguing every hosted provider; newt's probe
   measures instead.
5. **Dual-write JSONL+SQLite with prefer-longer-source reads**
   (session.py:1255-1360) — fossilized migration risk that spawned its own bug
   class (hermes #860). Migrate once; one truth.
6. **Gateway session keys + handoff state machines** (session.py:600-665;
   run.py:3824-3935). Cross-device continuity is Phase 16's (mesh) problem.
7. **The 13-section summary template and the curator's `max_iterations=9999`
   consolidation fork** (context_compressor.py:840-893; curator.py:1702-1715).
   Both assume frontier models; an unbounded autonomous consolidation agent is
   the opposite of newt's confined-loop philosophy (`max_tool_rounds`,
   lib.rs:3955-3968).
8. **Monolith accretion.** Hermes's compression triggers live at five call
   sites across a 16,408-line `run_agent.py` (12402, 14263, 14397, 14554,
   15335) plus a 1,583-line compressor. Newt's small-modules discipline and
   the 9.7 extraction are the assets that make this whole plan reviewable —
   route all loop-adjacent logic through `newt-core::agentic`.

---

## 4. Decision-doc reconciliation

This plan **amends** `docs/decisions/conversation_context_architecture.md`
(accepted earlier) in three places; the first Phase 17 PR updates that doc:

1. **Storage:** the decision doc chose `FileStore` (`turns.jsonl`) as the
   bootstrap floor. Superseded by SQLite (P17 argument above) — the JSONL
   floor predates the recall requirement.
2. **Trait:** the doc proposes a *trait* named `ConversationStore` plus a
   `JournalProvider` MemoryProvider wrapper (doc §"the trait"). Deferred —
   newt-core already has a concrete struct of the same name (a collision the
   first PR would hit head-on), and per the roadmap's own YAGNI standard the
   trait earns its keep only when a second store (Phase 16 `MeshStore`) is
   real. The Phase 16 design should target the concrete store's API; the
   trait can be extracted then.
3. **Step numbering:** the doc's "15.1-15.6" collide with ROADMAP Phase 15
   (Role Profiles); superseded by Phases 17-19 here.

Eval ephemerality (the doc's own requirement, kept): `--ephemeral` /
`NEWT_CONVERSATION_ID` land in 17.7 and the eval runner defaults to ephemeral.

## 5. Measurement

Every phase has before/after numbers defined **in advance** in
[`docs/testing/context-memory-benchmark.md`](../testing/context-memory-benchmark.md)
— baseline captured before 17.1a lands, kyln-benchmark style (quantified
TL;DR, honest interpretation, reproduce instructions, citability checklist).
