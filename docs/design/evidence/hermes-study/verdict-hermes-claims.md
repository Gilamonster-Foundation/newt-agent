VERDICT REPORT — hermes-side claims verified against /mnt/agent-workspace/hermes-agent (all paths relative to that root).

## Refuted or imprecise claims

1. **REFUTED (off-by-one, repeated 3x): "14-section summary template."** The shared template at agent/context_compressor.py:840-893 has exactly **13** `##` sections (Active Task, Goal, Constraints & Preferences, Completed Actions, Active State, In Progress, Blocked, Key Decisions, Resolved Questions, Pending User Asks, Relevant Files, Remaining Work, Critical Context). Correction: say "13-section template" in the gap matrix, WS-B, and Do-Not-Copy #7. The substance (too many sections for small models) stands.

2. **REFUTED: hermes has a "first non-empty wins" composition wart (Do-Not-Copy #1).** Hermes's MemoryManager **concatenates all** non-empty provider blocks, labeled per provider — `build_system_prompt` joins them with `"\n\n"` (agent/memory_manager.py:264-281) and `prefetch_all` does the same (285-300+). The only first-wins behavior is tool-*name* conflict routing (memory_manager.py:231-241), which is a different thing. Correction: the wart is newt's own (memory.rs:245-257 per the plan); drop the attribution to hermes or re-cite it as tool-name conflict handling.

3. **REFUTED (partially): "Hermes has no equivalent" to budget unification with probe tuning (WS-B).** Hermes's compressor budget *is* fed by its discovery system: on context-length-exceeded the probe step-down calls `compressor.update_model(context_length=new_ctx)` (run_agent.py:14512-14519), and `threshold_tokens` derives from that context_length (context_compressor.py:1452-1460 logs "Model context limit … threshold"). What hermes genuinely lacks is a *persisted tuning file* like newt's `model-capabilities.json`. Correction: narrow the claim to "hermes has no persisted per-model tuning store; the discovery→compressor linkage itself is standard hermes practice."

4. **FIT-PROBLEM (citation): compression triggers "at four call sites (12372, 15302, 14220, 10640)" (Do-Not-Copy #8).** 10640 is not a trigger — `_compress_context` is *defined* at run_agent.py:10656. Actual call sites: 12402 (preflight), 14263 / 14397 / 14554 (error-path: 413, long-context gate, ctx-exceeded), 15335 (usage trigger). The monolith-accretion point stands (run_agent.py = 16,408 lines; context_compressor.py = exactly 1,583), but the list should be 12402/14263/14397/14554/15335.

5. **FIT-PROBLEM (overstated): "no DB row until the first successful turn" (WS-A, lazy row creation).** The row is deferred from `__init__` (run_agent.py:1963 — "DB row deferred to run_conversation()") to the **start** of the first turn — `_ensure_db_session()` is called at run_agent.py:12126 before the turn runs, with retry-on-failure (2548-2568). It prevents ghost rows only for agents that never run a conversation, not for turns that start and fail. Minor, but the newt port should decide which semantic it actually wants.

6. **FIT-PROBLEM (citation drift): "Hermes splits MEMORY.md/USER.md (memory_tool.py:55-57)."** Lines 55-57 are `get_memory_dir()`. The split is documented at memory_tool.py:5-9 and implemented in `_path_for` (179-183). Mechanism real; citation wrong.

7. **FIT-PROBLEM (citation drift): the quote "tool schema bloat and conflicting memory backends" (Do-Not-Copy #1)** is at memory_manager.py:8 (module docstring), not in the cited :204-228 range (which contains the one-external-provider rule itself, confirmed). Quote verbatim: "prevents tool schema bloat and conflicting memory backends."

## Mechanisms the plan missed

8. **MISSING: auto-title generation.** agent/title_generator.py (`generate_title`, `maybe_auto_title`) creates a 3-7-word session title from the first exchange, asynchronously after the first reply ("never adds latency", title_generator.py:1-30), invoked at gateway/run.py:15838-15869. The plan's WS-A *consumes* titles everywhere — `conversations.title` column, tip-projection in `/conversation list`, the resume banner "[resumed: \<title\>, …]", recall browse results — but nothing in the plan ever *produces* a title. This belongs in WS-A (A4 or A7), even if newt's version is "first 60 chars of first user message" (hermes also keeps that `preview` fallback, hermes_state.py:1260-1265, 1306-1311).

9. **MISSING: secret redaction in the compression pipeline.** Hermes applies `redact_sensitive_text` to all summarizer input — message content at context_compressor.py:725 and tool-call args at :746 — plus the preamble instruction "NEVER include API keys… replace any that appear with [REDACTED]" (:833-836) and the Critical Context section's redaction rule (:893). The plan adopts the write-time security scan for memory (WS-C) but WS-B's summarizer has no redaction step, even though summaries are persisted into transcripts/DB and re-injected for the life of the conversation — the same exposure class the plan worries about for NOTES.md.

10. **MISSING (minor): token-budget tail protection.** Hermes protects the recent tail by token budget rather than fixed message count — `_find_tail_cut_by_tokens` (context_compressor.py:1292-1300+), `protect_tail_tokens` in the prune pass (:519-595), called out as a headline v2 improvement (:13 "Token-budget tail protection instead of fixed message count"). The plan's WS-B B3 specifies head anchors and the last-user anchor but never says how the tail boundary is sized; on weak local models a count-based tail with a few huge tool results defeats the whole pipeline.

11. **MISSING (minor): boundary alignment past tool_call/result groups.** `_align_boundary_forward` (context_compressor.py:1178+) and `_align_boundary_backward` (referenced in the #10896 docstring at :1253) *prevent* splitting tool pairs when computing the cut, before the post-hoc sanitizer fixes whatever's left. The plan ports only the post-repair (`_sanitize_tool_pairs` / newt's `repair_orphaned_tool_calls`). Prevention-then-repair is the hermes design; worth one line in B3.

## Confirmed claims (evidence)

12. **CONFIRMED:** Pre-send guard with compress-up-to-3-passes loop — run_agent.py:12372-12431 (`for _pass in range(3)` at 12400, break on no-shrink at 12406-12407, re-estimate at 12424-12431).

13. **CONFIRMED:** Real prompt_tokens for triggers, completion/reasoning excluded (thinking models, #12026), stale-zero fallback to rough estimate (#2153) — run_agent.py:15311-15331 (esp. 15317-15323); `update_from_response` stores prompt/completion separately at context_compressor.py:488-491.

14. **CONFIRMED:** Tool-schema accounting, "schemas alone can add 20-30K tokens" — model_metadata.py:1805-1825 (quote at 1814-1816); also counted in the preflight guard (run_agent.py:12377-12383, #14695 at 15325-15328).

15. **CONFIRMED:** chars/4 with `(len+3)//4` ceiling + flat 1,500-token image cost; no exact tokenizer anywhere (no tiktoken import in agent/ or run_agent.py) — model_metadata.py:1717-1726, 1729-1743.

16. **CONFIRMED:** Structural pruning — md5 hash-dedupe of >200-char tool results (context_compressor.py:597-621, hash at 615), per-tool one-line summaries incl. the exact `[terminal] ran `npm test` -> exit 0, 47 lines output` example (:224-343, example at 233), JSON-aware arg shrinking that parses/truncates-inside/reserializes, with MiniMax 400-loop issue #11762 cited (:178-221, esp. 185-194), applied in pass 3 (:659-683).

17. **CONFIRMED:** Tool-pair sanitization with stub-result insertion (orphaned results removed, stub `"[Result from earlier conversation — see context summary above]"` inserted for orphaned calls) — context_compressor.py:1118-1176.

18. **CONFIRMED:** Active Task verbatim ("Copy the user's most recent request … verbatim — the exact words they used") — context_compressor.py:840-846.

19. **CONFIRMED:** Iterative summary-of-summary ("PREVIOUS SUMMARY: / NEW TURNS TO INCORPORATE: … continue numbering") — context_compressor.py:899-913; cross-restart rehydration via prefix-scan of restored transcript — :1439-1448 (and `_find_latest_context_summary` :1093-1105).

20. **CONFIRMED:** Anti-thrashing, <10% savings twice → stop, with user-pointing message ("Consider /new … or /compress <topic>") — context_compressor.py:493-513; effectiveness tracked at :1565-1571.

21. **CONFIRMED:** Last-user-message anchor `_ensure_last_user_message_in_tail` (bug #10896 in docstring) — context_compressor.py:1245-1290; `[CONTEXT COMPACTION — REFERENCE ONLY]` prefix at :37-51; `--- END OF CONTEXT SUMMARY ---` marker for weak models, issues #11475/#14521 — :1527-1537 (and merge-into-tail path :1544-1549).

22. **CONFIRMED:** Static fallback marker "Summary generation was unavailable. N message(s) were removed…" — context_compressor.py:1489-1503.

23. **CONFIRMED:** Aux summary model with fallback-to-main and cooldown timers — context_compressor.py:766-791 (fallback bookkeeping), :1048-1070 (retry-on-main + 30/60s transient cooldown).

24. **CONFIRMED:** Manual `/compress [focus]` with before/after message+token feedback, noop detection, and the "fewer messages can still raise this estimate" honesty note — gateway/run.py:11088-11204 (focus extraction at 11096, feedback assembly 11188-11204); agent/manual_compression_feedback.py:8-49 (note at 38-42).

25. **CONFIRMED:** @-references with 25% soft / 50% hard budget gates — agent/context_references.py:132-203 (limits at 167-186).

26. **CONFIRMED:** Anthropic prompt-cache breakpoints (system + last 3, up to 4 markers) — agent/prompt_caching.py:49-79.

27. **CONFIRMED:** Pluggable context-engine, config-driven, one active — agent/context_engine.py:1-26 (selection at :9-10).

28. **CONFIRMED:** Probe-tier discovery + parse-limit-from-error — model_metadata.py:877-910 (`CONTEXT_PROBE_TIERS` at :118); provenance-gated persistence ("Only persist limits parsed from the provider's error message… Guessed fallback tiers… stay in-memory only") — run_agent.py:14520-14531.

29. **CONFIRMED:** 10-step context-length waterfall with hand-maintained tables and longest-key-first fuzzy defaults — model_metadata.py:1428-1714 (docstring resolution order 1436-1459, `# 10. Default fallback — 256K` at 1713, substring-match defaults 1699-1703); provider-specific cache-invalidation shims (Codex ≥400K, Kimi ≤32K, Nous bypass) — :1493-1529.

30. **CONFIRMED:** Frozen-snapshot memory (snapshot captured at load, never mutated mid-session, prefix-cache rationale) — tools/memory_tool.py:11-14, 107-124, 358-369.

31. **CONFIRMED:** Over-budget add returns full entry list + "Replace or remove existing entries first." with `current_entries` and usage — memory_tool.py:247-258. Substring replace/remove with "Multiple entries matched … Be more specific." ambiguity errors and `§` delimiter — :59, :266-356. Usage header `[{pct}% — {current:,}/{limit:,} chars]`, default limit 2,200 — :118, :390-406.

32. **CONFIRMED:** Write-time security scan: invisible-unicode set + injection/exfiltration regex table, with "Memory entries are injected into the system prompt" rationale — memory_tool.py:67-104 (rationale at :102). Sidecar `.lock` + flock (:144-176) and atomic temp-file+rename write (:430-459).

33. **CONFIRMED:** Agent-writable memory tool dispatch in the agent loop — run_agent.py:11594-11618; "Do NOT save task progress…use session_search" in the schema — memory_tool.py:526-527 (within cited 518-535).

34. **CONFIRMED:** Turn-counted nudge: default interval 10 (run_agent.py:1997), counter reset on organic `memory` tool use (:11055-11057), trigger check per user turn (:12291-12301), modulo hydration after agent-cache miss (`prior_user_turns % interval`, #22357) (:12246-12265).

35. **CONFIRMED:** Background review fork — daemon thread (`threading.Thread(target=_run_review, daemon=True, name="bg-review")` at run_agent.py:4502), thread-local tool whitelist (:4398-4417, 4429), inherited byte-exact cached system prompt with "~26% end-to-end cost reduction on Sonnet 4.5" / PR #17276 (:4377-4387), recursion guard via zeroed nudge intervals (:4367-4368).

36. **CONFIRMED:** Anti-rot rule, near-verbatim: "Negative claims about tools or features … harden into refusals the agent cites against itself for months" — run_agent.py:4119-4128 (within cited 4049-4143, the skill-review prompt).

37. **CONFIRMED:** Memory prompt guidance "Write memories as declarative facts, not instructions to yourself" and "If a fact will be stale in a week, it does not belong in memory" — agent/prompt_builder.py:150-171 (quotes at 165, 162).

38. **CONFIRMED:** Skills curator with declare-intent-at-delete `absorbed_into` reconciliation — agent/curator.py:695-720 (+ prompt requirements at 380-440); `max_iterations=9999` consolidation fork with its own zeroed nudges — :1702-1715.

39. **CONFIRMED:** SQLite-primary schema with `parent_session_id`, `end_reason`, per-message `tool_calls`/`tool_name` columns and indexes — hermes_state.py:185-251; schema-diff migration with the exact quoted behavior "Adding a column to SCHEMA_SQL is all that's needed; the reconciliation loop picks it up automatically" — :463-505 (quote 473-474).

40. **CONFIRMED:** FTS5 dual-index (default-tokenizer table + trigram CJK table) trigger-maintained over `content || tool_name || tool_calls` — hermes_state.py:253-306; `snippet()` usage at :1935, :2004. (Plan's "skip the trigram table" is a fair fit call; note the default table is implicit unicode61, so "one unicode61 table" matches hermes's default.)

41. **CONFIRMED:** Query sanitizer — balanced-phrase preservation, special-char stripping, dangling AND/OR/NOT trim, auto-quoting of dotted/hyphenated tokens with the literal `chat-send` / `P2.2` examples — hermes_state.py:1797-1847.

42. **CONFIRMED:** Compression lineage + tip-projection + recursive-CTE MRU in SQL — hermes_state.py:1162-1350 (CTE at 1237-1278, projection 1316-1348); resume redirect walking descendants to the row-bearing child (#15000) — :1621-1684; MRU `last_active = MAX(message ts) else started_at` — :2163-2185.

43. **CONFIRMED:** session_search: empty query = zero-LLM recent list (tools/session_search_tool.py:268-322, dispatch at 359-362), FTS (373-379), lineage dedupe (393-430), parallel aux-LLM recaps via `asyncio.gather` (198-259, 468-474), ±1 context message with full content dropped — "snippet is enough, saves tokens" verbatim at hermes_state.py:2145 (:2083-2147), coaching schema text "USE THIS PROACTIVELY when… 'remember when'" and "Use OR between keywords" (session_search_tool.py:561-571).

44. **CONFIRMED:** Stable key → rotating id — `build_session_key` (gateway/session.py:600-665), fresh `session_id` minted per reset (:918), `/resume` re-points the key via `switch_session` + `reopen_session`, transcripts never destroyed (:1182-1235).

45. **CONFIRMED:** `is_fresh_reset` vs `was_auto_reset` distinction, issue #6508, exactly to avoid the misleading auto-reset notice on manual `/new` — gateway/session.py:462-469; interruption model (`suspended` hard-wipe wins, `resume_pending` preserves id, strike escalation) — :458-492, :856-955 (suspended-wins at 887-898), :973-1128; `_STUCK_LOOP_THRESHOLD = 3` at gateway/run.py:~3004; startup auto-resume scheduler — gateway/run.py:3229-3295.

46. **CONFIRMED:** Dual-write JSONL+SQLite with #860 duplicate-write scar (`skip_db` at gateway/session.py:1255-1263) and prefer-longer-source read guard (:1314-1360). Plan's "migration scar tissue" framing matches the in-code rationale ("legacy session not yet fully migrated").

47. **CONFIRMED:** Handoff state machine — pending/claim/complete/failed watcher (gateway/run.py:3824-3884) and `_process_handoff` with home-channel resolution (:3884-3935+). Gateway-shaped as claimed.

48. **CONFIRMED:** WAL→DELETE fallback on exactly "locking protocol" / "disk i/o error" markers, with init error captured for user-facing messages — hermes_state.py:54-57, 105-125, 128-161; jittered application-level write retry (15 retries, 20-150ms, anti-convoy rationale) — :317-446. Plan's "skip the 15-retry convoy machinery, keep the WAL fallback" maps cleanly onto these two distinct mechanisms.

49. **CONFIRMED:** Honcho is a hosted-service memory plugin with per-peer representations and dialectic LLM supplements — plugins/memory/honcho/__init__.py:239-247 (config/availability), :547-560 (prefetch = peer representation + dialectic supplement), peer-card/dialectic tooling at :39-121; one-external-provider rule that contains it — memory_manager.py:204-228.

**Bottom line:** of ~50 distinct hermes-side mechanism claims, 42 are confirmed at or within a few lines of the cited locations, with verbatim quotes checking out in every case I tested. The substantive errors are #1 (13 not 14 sections), #2 (hermes does not have the "first non-empty wins" composition wart), and #3 ("hermes has no equivalent" budget-unification overclaim); #4-#7 are citation-level imprecisions. The plan's biggest real gaps are #8 (no title producer for a plan whose UX surfaces depend on titles) and #9 (no secret redaction in the WS-B summarizer despite adopting the equivalent scan for memory).