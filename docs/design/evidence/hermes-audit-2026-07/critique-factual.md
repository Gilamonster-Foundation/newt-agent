# Fact-check report: hermes-audit-2026-07.md vs primary sources

All hermes citations checked against `hermes-main` @ `830165473` (confirmed HEAD). Newt-side claims checked against `newt-agent` and the Gilamonster-Foundation/newt-agent tracker.

## The (a)–(j) load-bearing claims

**(a) session_search "No LLM calls anywhere" + retirement — TRUE.**
`tools/session_search_tool.py` docstring, lines 21–23: "No LLM calls anywhere — every shape returns actual messages from the DB." History paragraph (lines 25–30) confirms PR #20238 seeded a "fast/summary dual-mode split" and the module now has "no summary LLM path". Cite `:23-30` is accurate. The bookends (first/last 3 messages) and demote-not-exclude cron ranking with #19434 "recall blindness" (doc A7) are also verbatim in this file (lines 8–11, 43–52).

**(b) In-place compaction active/compacted + FTS searchability — TRUE.**
`hermes_state.py:766-767` defines `active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0`. `archive_and_compact()` (`hermes_state.py:3651` region) flips `active=0, compacted=1` in one transaction, cites #38763 ("one session id for life"), and explicitly documents that archived rows "remain in the FTS index (the messages_fts* triggers... don't key on active/compacted)", are included by `search_messages()` by default, and recoverable via `include_inactive=True`. Tri-state rewind semantics (active=0, compacted=0 = "user took it back") confirmed. `compression.in_place` config exists (`agent/conversation_compression.py:485-494`, default False). The doc's "landing"/rollout framing correctly reflects the default-off state. The orphaned-child-session bug cluster is real (orphan-avoidance rollback code at `conversation_compression.py:512, 785-847`) and the `compression_locks` cross-process table exists (`hermes_state.py:783`). One nit: the doc's quoted phrase "spawned the orphaned-child-session bug cluster" is the *survey report's* wording (context-compression.md:42), not a hermes source string — the mechanism is real but the quotation marks over-attribute.

**(c) #7915 no-budget-warnings — TRUE.**
`agent/agent_init.py:624-629`: "the LLM is only notified when it actually exhausts the iteration budget... No intermediate pressure warnings — they caused models to 'give up' prematurely on complex tasks (#7915)." The doc's word "removed" is an inference from the past-tense comment (no commit mentioning 7915 is findable in the clone's log), but the survey and comment both support prior existence + reversion. The quote itself is genuine source text.

**(d) verification_stop / verification_evidence — TRUE.**
`agent/verification_evidence.py`: `classify_verification_command()` matches terminal commands against project `verifyCommands`, records pass/fail with `output_summary` into SQLite (`verification_events`/`verification_state` tables). `agent/verification_stop.py` is explicitly "policy-only. It never runs checks itself"; `build_verify_on_stop_nudge(max_attempts=2)` — the ≤2 bound is real; `_status_detail()` quotes the last failing output (up to 1,200 chars). `conversation_loop.py` ~5125–5155 integration confirmed: refuses the final answer (`finish_reason="verification_required"`), and both the attempted answer and nudge are flagged `_verification_stop_synthetic` "so neither persists" (#55733) — matching "non-persisted". Doc/markdown-only edits suppress the nudge, as the doc implies via "when code was edited".

**(e) execute_code RPC — TRUE.**
`tools/code_execution_tool.py`: generated `hermes_tools.py` stub module (line 272: "Build the source code for the hermes_tools.py stub module", only tools in both `SANDBOX_ALLOWED_TOOLS` ∩ enabled tools); Unix domain socket RPC listener (lines 11–12, POSIX-only, file-based fallback transport for remote backends); `DEFAULT_MAX_TOOL_CALLS = 50` enforced at line ~564; each call is "Dispatch[ed] through the standard tool handler" (line ~580); stdout-only return with `MAX_STDOUT_BYTES = 50_000` and tool description "Print your final result to stdout" + "Limits: 5-minute timeout, 50KB stdout cap, max 50 tool calls per script" (lines 1849–1868 as cited). The round-refund the doc warns against is also real: `conversation_loop.py:4708-4713` refunds iterations when the only tool called was `execute_code`.

**(f) delegate_tool supervision — TRUE (all three).**
- Headroom-scaled summary caps: `delegate_tool.py` ~1620–1700 — "Per-summary character budget sized against the parent's *remaining* context headroom, split across the batch", `cap = min(static ceiling, dynamic headroom budget ÷ batch size)`, explicitly cites #9126; `_trim_summary_with_footer` (line 1569) does head+tail (75/25) trim, spill to disk, and a `read_file offset=` paging footer.
- Reject-not-queue: `async_delegation.py` ~154–198 — "When at capacity the dispatch is REJECTED... rather than queued, so a runaway model can't pile up unbounded background work", capacity check + insert under one lock. The doc's "falling back to synchronous with a note" is also real: `delegate_tool.py` ~2870–2885 runs the batch synchronously on pool-at-capacity and attaches an explanatory `note`.
- Progress-based staleness: `delegate_tool.py` heartbeat loop (~1767+) tracks (current_tool, api_call_count) advancement with separate idle vs in-tool thresholds; `_get_child_timeout()` docstring (lines ~425–441) confirms no wall-clock default and "Stuck-child protection is handled separately by the heartbeat staleness monitor, which stops refreshing parent activity so the gateway inactivity timeout can fire."

**(g) Velocity numbers — TRUE (one minor discrepancy).**
Recomputed in `hermes-main`: `git log --since=2026-06-06 --oneline | wc -l` = **3,886** (exact match). Commits with subject starting `fix` = 2,092/3,886 = **53.8% ≈ 54%** (match). Unique authors = **569** by name, **571** by email — the doc's **566** is slightly low (~1% off; likely a counting-method artifact). Immaterial but not exact.

**(h) Schema v19 — TRUE.**
`hermes_state.py:125`: `SCHEMA_VERSION = 19`. The "8 migrations in a month" arithmetic depends on the June study's v11 baseline, which is not verifiable from this checkout (UNVERIFIABLE baseline, arithmetic consistent).

**(i) Cron wake gate — TRUE.**
`cron/scheduler.py:2041-2064` `_parse_wake_gate()`: last non-empty stdout line parsed as JSON; `{"wakeAgent": false}` → "the agent is skipped entirely — no LLM run, no delivery"; anything else wakes normally. The empty-output skip is also real (~2107: "Script produced no output — nothing to report, skip AI call"). Cited lines match.

**(j) One-blocking-lint-rule + no coverage gate — TRUE.**
`pyproject.toml:378-386`: `select = ["PLW1514"]` with the comment "All other lints are intentionally disabled (see comment history on this file) while we wrangle typechecks — but PLW1514 is too load-bearing to keep off." The doc's quote is accurate (lightly elided). No coverage gate: grep across all 16 `.github/workflows/*.yml` finds no `--cov`/coverage enforcement (only an unrelated comment in docker.yml). Subprocess-per-file test isolation confirmed: `tests/conftest.py:37` — "``python -m pytest <file>`` subprocess per test file" via `scripts/run_tests_parallel.py`.

## Newt-side claims

**progressive-disclosure-compaction.md gap — TRUE.** Lines 63–64 of that doc: "The gap is narrow and specific: **the compaction marker does not carry the `memory_fetch` handles.** The keys exist; eviction simply never hands them" — exactly as the audit doc quotes it.

**#950 family defaults — TRUE.** newt-agent #950 "feat(cards): family-level vLLM serving defaults (reasoning/tool-call parser)" — MERGED. Also confirmed: #942 "fix(agentic): live-session round-cap grace..." (MERGED — matches "grace-rounds design (#942)"), #945 head+tail output capping (OPEN), #948 subagent tool (OPEN). The doc's "landed this very week" is accurate for #942/#950; #945/#948 are open (doc says #948 "already filed", which is correct).

## Other spot-checked concrete claims (all TRUE unless noted)

- God-file sizes: `gateway/run.py` = 20,526 lines (doc: 20.5K ✓); `cli.py` = 16,184 (doc: 16.2K ✓); `context_compressor.py` = 3,082 (doc: exact ✓).
- "~4,700-line turn-loop function": `run_conversation` at `conversation_loop.py:518` is the last top-level def in a 5,294-line file → ~4,777 lines ✓.
- LOC: src .py = 636,411 (doc: 635K ✓); tests .py = 710,928 (doc: 711K ✓).
- `gateway_routing` DB table as "durable replacement" for sessions.json: `hermes_state.py:775, 1735` ✓.
- resume_pending freshness gate default 1h: `gateway/session.py:36` `_AUTO_CONTINUE_FRESHNESS_SECS_DEFAULT = 60 * 60` ✓.
- Persisted compression cooldowns: `hermes_state.py:2098` ✓.
- Prefix-cache invariant: `system_prompt.py:113-130` — three-part prompt "cached on agent._cached_system_prompt... Hermes never re-renders parts of this string mid-session — that's the only way to keep upstream prompt caches warm" ✓.
- Temporal anchoring: `context_compressor.py:1742-1758` — "Never leave a finished action worded as if it still needs doing" is verbatim source ✓.
- Per-turn aggregate budget spilling largest first: `tool_result_storage.py:203-254` `enforce_turn_budget` ("persist the largest non-persisted results first") ✓.
- `read_file` threshold pinned to infinity: `budget_config.py:10-13` `PINNED_THRESHOLDS = {"read_file": float("inf")}` — "prevents infinite persist->read->persist loops" ✓.
- Streaming head+tail drain: `code_execution_tool.py:1364+` (40% head budget confirmed) ✓.
- Paging-hint truncation footer: `web_tools.py:530-552` ✓.
- Guard canary self-test: `tests/test_live_system_guard_self_test.py` exists; "(5+ live gateway kills in 3 days)" in its docstring ✓. `history-check.yml` workflow exists ✓.
- Negative-space capture taxonomy: `background_review.py:250-269` — "harden into refusals the agent cites against itself for months" is near-verbatim ✓; "Be ACTIVE" at line 282 ✓.

## Claims stated more strongly than the source supports

1. **"a 4,300-line curator to garbage-collect it" (AVOID-9) — MISLEADING as phrased.** No 4,300-line curator exists. Non-test curator modules total **3,385 lines** (`agent/curator.py` 1,976 + `agent/curator_backup.py` 711 + `hermes_cli/curator.py` 698). The survey (skills.md:60) said "~4,300 lines" for the whole *subsystem* — "curator, consolidation pass, pin system, protected-builtin list, and backup/rollback" — which is plausible only if slices of `skill_manager_tool.py` (1,542 lines) are counted. The doc compresses this into a single-artifact figure. Suggest "≈3,400-line curator/backup subsystem (plus consolidation machinery in skill_manager_tool)".
2. **"566 authors" — minor undercount.** Recomputed: 569 (unique names) / 571 (unique emails). Say "~570".
3. **Quoted phrase "spawned the orphaned-child-session bug cluster"** — this is survey wording, not a hermes source string; the doc's quotation marks imply a primary-source quote. The underlying mechanism (orphan rollback code + `compression_locks` table) is fully real.
4. **"Hermes removed mid-flight budget-pressure warnings (#7915)"** — the removal *event* is inferred from the `agent_init.py:624-629` comment's past tense ("they caused models to..."); no commit referencing #7915 is reachable in this clone. The design fact and the quote are solid; "removed" vs "deliberately does not have" is a mild inferential step.

Everything else checked resolves TRUE with citations matching the mechanism (line numbers occasionally ±10, as the doc itself warns). No FALSE claims found.