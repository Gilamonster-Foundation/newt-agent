Verdict report — NEWT-side claims verified against /mnt/agent-workspace/newt-agent (HEAD 6b7d780). Hermes-side citations not checkable from this repo and are out of scope.

**Sequencing / integration spine**

1. **REFUTED (overstated): "Phase 9.7 … is in flight."** Step 9.7 exists only as a merged roadmap spec (docs/ROADMAP.md:552-581, landed via docs commits 743ce2a/#213 and 8d0c914/#219). There is no `newt-core/src/agentic/` module (module list at newt-core/src/lib.rs:7-24), no `step-9.7-*` branch, and even Step 9.6's eval cases are absent (`newt-eval/cases-deferred/` contains only `006-cross-host-rename`). Correction: 9.7 is *specced, not started*. The dependency logic ("loop changes land after 9.7") still holds, but the plan should not assume 9.7 lands soon without scheduling it.

2. **CONFIRMED (citation off by one step): dual Ollama/OpenAI loops.** `chat_complete` at newt-tui/src/lib.rs:3571 delegates OpenAI to `openai_chat_complete` at lib.rs:4158 — both exact. But the cited ROADMAP range ":529-626" starts at Step **9.6** (ROADMAP:529); Step 9.7 itself is 552-581.

3. **CONFIRMED: Phase-number collision.** Decision doc steps 15.1-15.6 (docs/decisions/conversation_context_architecture.md:363-374) collide with ROADMAP "Phase 15 — Role Profiles" at exactly ROADMAP:896 (its own "Step 15.1 — RoleProfile" at :909). Phase 16 Mesh at ROADMAP:958, also exact.

**Context-window management**

4. **CONFIRMED: guard exists but discards.** `trim_to_token_budget` lib.rs:2705-2721; pre-send guard lib.rs:3692-3706; mid-loop trim lib.rs:3669-3687; middle replaced by a single placeholder user message lib.rs:2666-2671. No summarizer anywhere on this path.

5. **REFUTED in part: "chars/4 everywhere (lib.rs:2688-2694; memory.rs:511-514)."** The loop estimate is chars/4 (lib.rs:2688-2694, exact). But the memory providers already **prefer real backend-reported usage** and fall back to len/4 only when absent: `metrics.usage.map(|u| u.input_tokens + u.output_tokens)` at memory.rs:507-514 (TokenBudget) and :888-892 (Summarizing). The real defect is different from what WS-D describes: newt sums input+output **per turn** and accumulates across history, so prompt tokens (which already include all prior turns) are massively double-counted as history grows. WS-D should target that, not "no real usage at all."

6. **CONFIRMED: tool schemas not counted.** `estimate_tokens` sums only messages (lib.rs:2688-2694) while the dispatched body separately includes `"tools": merged_tool_definitions(mcp)` (lib.rs:3712-3727).

7. **CONFIRMED: probe ratchet is as described.** Confidence ratchet probe.rs:126-189; parsed-400 limits persisted via `record_context_window_400` probe.rs:156-173; 80%-headroom-absorbs-estimate-error doc probe.rs:149-153; `safe_context`/`max_ok_input` fields probe.rs:74-108. All exact.

8. **CONFIRMED with mischaracterization: `repair_orphaned_tool_calls` (lib.rs:2791-2858, exact).** But the plan's rider — "so assistant turns survive instead of being stripped" — misdescribes current behavior: assistant **messages already survive**; only their `tool_calls` field is removed, with a `"[tool calls omitted]"` content stub inserted (lib.rs:2831-2837); orphaned `role:"tool"` results are then dropped (lib.rs:2848-2858). The hermes stub-result variant would preserve the *tool_call/result pairing*, not the assistant turn.

9. **CONFIRMED: Summarizing provider's three defects.** (a) placeholder without summarizer memory.rs:833-839; (b) blocking-HTTP-inside-`sync_turn`: closure does `block_in_place` + `block_on` reqwest at lib.rs:1290-1305, invoked from `compress_sync` inside `sync_turn` (memory.rs:898-900), against the trait doc "queue writes and return immediately" (memory.rs:140-144); (c) never called from the TUI trim path (trim_for_summary lib.rs:2655-2678 is summarizer-free). `with_summarizer` injection at lib.rs:1263-1314, exact.

10. **CONFIRMED: `prev_summary` wiped on restore.** Built same-process at memory.rs:799-804/851; `self.prev_summary.clear()` at exactly memory.rs:919, followed by immediate possible re-compression with no prior summary (memory.rs:922-924).

11. **CONFIRMED: anti-thrash exists only inside `Summarizing`** (memory.rs:774-785). No equivalent in `mid_loop_trim` / `trim_to_token_budget`.

12. **CONFIRMED: head-keeping implicit, no anchors.** Callers pass `head: 2` (lib.rs:3693, :2738, :2743); no last-user-message tail anchor, no END-OF-SUMMARY marker exists.

13. **CONFIRMED: no manual `/compress`.** Slash dispatch (lib.rs:4545+) and the pre-dispatch handlers (lib.rs:1392-1477) have memory/remember/new/conversation/persona/help/version/workspace/models — no compress. **FIT-PROBLEM for B5:** there is also **no `/status` command** to surface anti-thrash counters in; the existing surface is `/memory` (lib.rs:1393-1405). B5 should target `/memory` or create `/status`.

14. **MISSING from the plan (already exists): a 5-section lean summary template.** WS-B proposes designing a "lean ~6-section template" as if from scratch, but `Summarizing` already prompts for exactly Active Task / Completed Actions / Key Decisions / Relevant Files / Critical Context with "Be terse. Preserve specifics" (memory.rs:814-819). WS-B is a relocation + 1 section, not new design.

**Memory**

15. **CONFIRMED: frozen-snapshot semantics** — trait doc memory.rs:113-117; NoteStore freeze memory.rs:562-568, 667-668.

16. **CONFIRMED: over-budget add is a bare hard fail** — `bail!` with counts but no entry listing, no guidance (memory.rs:611-617).

17. **CONFIRMED: zero agent-initiated memory writes.** `/remember` human-only (lib.rs:1407-1414); the model's tool set is exactly run_command/read_file/write_file/edit_file/list_dir/use_skill/web_fetch (lib.rs:2473-2569) plus MCP — no memory tool.

18. **REFUTED: "Usage header — newt: None."** NoteStore's system-prompt block already renders `## Agent Notes ({used}/{limit})` (memory.rs:676-681), and `/memory` prints percentage usage (lib.rs:1395-1403). What's missing is only the percentage inside the block. The "trivial ADOPT" is a one-line format tweak to an existing header, not a new feature.

19. **CONFIRMED: no write-time security scan** — `add()` memory.rs:600-622 has none; snapshot injected verbatim memory.rs:672-682.

20. **CONFIRMED: nudge absent for memory** (no counter anywhere). Note: the loop already has an analogous in-band nudge mechanism — the read-only-rounds nudge with reset-on-use (lib.rs:3646-3664) — a ready pattern/precedent C4 should explicitly reuse.

21. **CONFIRMED: `add_note` string-name routing hack** at memory.rs:306-315 (`p.name() == "note_store"`), and `on_session_end` is a no-op everywhere — **stronger than claimed**: the trait default (memory.rs:167) is never overridden by any provider AND `MemoryManager::on_session_end` (memory.rs:293-297) is never called from the TUI at all.

22. **CONFIRMED with nuance: no file locking / atomic write in NoteStore** (plain `fs::write`, memory.rs:640-646). Nuance: the repo already has the write-then-rename idiom in `ConversationStore::save_record` (conversation.rs:200-215) — C1 should copy it, not invent it. The "memory.rs:580" cite for single-NOTES.md is slightly off (580 is `DEFAULT_CHAR_LIMIT`; path at 591-597), substance correct — no USER.md anywhere.

23. **CONFIRMED: skills index, names+descriptions only** — lib.rs:2077-2088, exact.

**Sessions / persistence / recall**

24. **CONFIRMED: pretty-JSON-per-conversation with whole-file rewrite per turn.** `to_string_pretty` conversation.rs:203, `.json` path :269-271; `append_turn` loads + rewrites the entire record every turn (conversation.rs:119-127).

25. **CONFIRMED: only `(user, assistant)` final text persisted.** `ConversationTurn` is two strings (conversation.rs:8-12); save path lib.rs:2268-2285; memory sync `(task, reply)` lib.rs:1616. Tool rounds and usage discarded. (A6's note that events live inside `chat_complete` and aren't returned to the caller is accurate — the loop returns only `(reply, streamed, usage, hallucinations)`, lib.rs:3574.)

26. **CONFIRMED: no search of any kind** — `/conversation` subcommands are list/show/restore/rename/delete only (lib.rs:1993-2030); **`prefetch` dead surface confirmed and understated**: no provider overrides it (only default memory.rs:125-127) and `prefetch_all` (memory.rs:227-239) has no TUI call site.

27. **CONFIRMED: fresh id every launch** — lib.rs:1175; `/new` mints another (lib.rs:2220-2233). No lineage: `ConversationRecord` has no `parent_id` (conversation.rs:23-35).

28. **REFUTED in part: "Prune/list by record timestamps only" → MRU is an ADOPT.** `append_turn` bumps `updated_at_unix_nanos` on **every turn** (conversation.rs:122), and list/prune sort by it (conversation.rs:139-141, 278-283) — so `updated_at` already behaves as last-active and the MRU mechanism substantially exists. Real gaps: no per-turn timestamps at all (conversation.rs:8-12), and `rename` also bumps `updated_at` (conversation.rs:157), polluting MRU order. The ADOPT should shrink to "per-turn timestamps + don't let rename count as activity."

29. **MISSING from the plan (already exists): lazy record creation.** WS-A lists "Lazy row creation — no DB row until the first successful turn" as stolen-from-hermes. Newt already does exactly this: the record is created only when the first successful turn saves (lib.rs:2275-2284, on the `Ok` arm at lib.rs:1600-1618; documented at conversation.rs:82-89). Carry the behavior into A1, don't re-design it.

30. **CONFIRMED: UUIDv5-over-canonical-path keying is path-fragile** — conversation.rs:70-74, exact. Partial-existence note: a stable per-workspace key + per-workspace conversation namespace already exists (`workspace_dir()` = root/conversations/<workspace_id>, conversation.rs:273-275); A2/A7's genuinely new pieces are the key *derivation* (remote+branch) and the key→active-conversation mapping.

31. **CONFIRMED: decision-doc claims** — blake3(git remote + branch) with path fallback at conversation_context_architecture.md:81-102 (plan cites 81-103, fine); manual-restore-only/"starts from zero" framing at :35-42.

32. **REFUTED (naming): "the decision doc proposes a `JournalProvider` trait (… :310-344)."** The doc's **trait** is named `ConversationStore` (md:317-324); `JournalProvider` is a concrete `MemoryProvider` wrapper over it (md:341-343). This matters because newt-core already has a *concrete struct* named `ConversationStore` (conversation.rs:47) — the plan's "keep ConversationStore a concrete struct" is the right call but it's rejecting the doc's same-named trait, and the first WS-A PR will hit this name collision head-on.

33. **CONFIRMED: budget disconnection (B1's target).** Providers built from `mem_cfg.context_tokens.unwrap_or(8_192)` (lib.rs:1260, :1264; default at memory.rs:462-469) while the loop's `send_budget` separately consumes `max_ok_input.or(safe_context)` from the cap cache (lib.rs:3624; ChatCtx fields lib.rs:3555-3565). Two parallel budget systems, as claimed.

34. **CONFIRMED: D2's restore re-estimation** — memory.rs:543 and :916 both recompute len/4 on restore; real usage can't be restored today because `ConversationTurn` persists no tokens (consistent with WS-A's schema extension being a prerequisite).

35. **CONFIRMED (minor): cap-exit no-tools pattern** — "No `tools` key => the model cannot emit tool calls" at exactly lib.rs:4024; cap-exit flow lib.rs:3955-3968. Test-count claims slightly inflated: memory.rs has 38 tests (plan: ~45), probe.rs 37 (plan: ~40).

**Net assessment:** the plan's load-bearing integration points (file locations, the MemoryProvider seam, trim/guard call sites, probe API) are accurate, mostly to the exact line. The systematic bias is **under-crediting newt**: five proposed adoptions already exist wholly or partly (lazy record creation #29, MRU-by-updated_at #28, notes usage header #18, lean summary template #14, real-token-usage preference #5), one rejected trait is misnamed (#32), and the sequencing premise "9.7 in flight" is not true in this repo (#1) — WS-B/A5-A7/C4-C5 have no landing zone until 9.7 is actually scheduled and built.