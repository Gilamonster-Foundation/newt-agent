# Review: hermes-audit-2026-07.md vs its 10-report evidence base

## 1. MISSED FINDINGS

**Real gaps (should be added):**

- **Write hygiene for agent-writable files** (memory-learning §candidate 6): threat-scan at write AND at snapshot load with visible `[BLOCKED]` placeholders, round-trip drift guard + `.bak` before full-file rewrite (`memory_tool.py:78-241,704-757`). Newt has a live write path (`save_note`/NoteStore, NOTES.md) and an operator who hand-edits these files; silent clobbering of hand edits is exactly the failure this prevents. A10-class, cheap, aligned with the authority model. Omission is a real gap — nothing in the doc covers agent-writable-file hygiene.
- **Typed error taxonomy + one-shot recovery guards** (agent-loop §candidate 3): `FailoverReason` enum → recovery-action mapping, `TurnRetryState` replacing scattered one-shot booleans. The report called it out as cleaner in Rust than in hermes. The doc adopts the recovery *ladder* (CONSIDER) but drops the taxonomy that makes the ladder testable. Deserves at least a CONSIDER line.
- **Flush-before-destructive-tool persistence + `_turn_exit_reason` on every exit path** (agent-loop §candidate 7): report says "both trivial to add" — to the session store and eval autopsy tooling respectively. These are precisely A10-shaped hygiene items; their absence from A10 looks like an oversight, not selection. `_turn_exit_reason` in particular feeds newt's autopsy/eval discipline directly.
- **Depth/authority-grounded child prompt** (subagents §candidate 5): "state the child's *actual* met-down caveat set in its prompt, computed from config, so it doesn't confabulate capabilities." Fits #948/A9 exactly and is one sentence to add to A9. Real (small) gap.
- **A7 drops half the ranking fix**: the report gives demote-not-exclude *plus* the 300-row scan-before-dedupe budget (`session_search_tool.py:54-58`) — interactive hits buried under automation walls still surface. The doc kept demote + bookends and silently dropped the scan budget, which is the part that makes demotion sufficient.
- **Conditional skill-index gating** (skills §candidate 5): `requires_tools`/`platforms` frontmatter + posture-based names-only demotion. newt-skills has an index renderer today; this is cheap token control independent of the (correctly-gated) write path. CONSIDER-class miss.
- **Dual-audience contribution rubric / when-NOT-to-close enums** (engineering-shape §candidate 3): proposed as workflow-steer TOML data for crew PR review — a live newt workstream (improving-crew-results). CONSIDER-class miss.

**Defensible omissions (no change needed):** MEMORY/USER two-budget split (settled-rejected, doc cites it); atomic-batch memory consolidation + terminal responses (newt already shipped the June curation cap; residual delta is small); migration-discipline pattern (conditional, report itself says "only if forced"); AsyncSessionDB facade avoid (moot in Rust); file-read-dedup/compaction coupling (conditional on a feature newt lacks); env-var policy plumbing avoid (newt's config/caveats already preclude); lane-gated CI (newt CI is small; report's argument is about scale newt doesn't have); anti-pattern glossary + conftest credential-stripping (nice-to-have; arguably foldable into A10 but their loss doesn't distort the picture); last-user-message tail anchor (newt already has verbatim-Active-Task + end markers per the ledger; only the structural tail guarantee is new — borderline, one clause in A5 would cover it); patch-first skill preference order (belongs inside the CONSIDER background-review item if pursued — one clause).

## 2. OVERCLAIMS-BY-OMISSION

- **A4 soft-archive**: the June docs already flagged the exact open questions this reopens — `compaction_archive` retention, **secret-retention adversarial review**, and `verify_chain` participation of archive rows (settled-verdicts §4). Keeping compacted turns FTS-searchable means secrets in raw turns stay recallable forever unless redact-on-archive is specified; newt's spill store redacts on store, the archive path as described doesn't. A4 should name these three open items instead of reading as a pure win.
- **A3 script-RPC**: "one new tool in `tools.rs`" undersells the largest build in the ADOPT list: typed stub generation, a confined child *process with a script runtime* (hermes generates **Python** stubs — what does a Rust-local-first newt child run?), a socket RPC server, per-run token, 50-call cap, timeout + process-tree kill, env scrubbing, stdout redaction. The runtime-language decision is unstated and is the actual design question. Also unnoted: A3 adds a permanent ~450-word core-tool schema to every call — in direct tension with the doc's own A10 Footprint Ladder ("new core tool last"). One sentence acknowledging the tension (and why it's worth paying) would fix it.
- **A1 prefix-cache**: presented as "the single highest-value import" without noting how much newt already has (frozen-snapshot memory and frozen-prefix discipline are settled-adopted per the June ledger — the doc's own settled section implies this). The actionable delta is narrower: byte-identical persist/restore on resume, miss diagnostics, canonicalized tool-call JSON, and the prune-pipeline audit. Also the agent-loop report explicitly notes hermes's "~75% cost reduction" is "a docstring claim, not measured" — the doc's payoff sentence inherits the optimism without the caveat.
- **A2 verify-on-stop**: drops two implementer-relevant caveats from the report: the code-vs-docs path filter (doc/prose edits must not trigger the gate) and the strip-synthetic-messages-from-persistence requirement (#55733 — resume adjacency + prefix poisoning). "Non-persisted" gestures at the second; the first is absent.
- **Delta table**: "sessions.json is a legacy mirror behind a flag" — the flag defaults to **ON** (`write_sessions_json` default True); "behind a flag" implies off. Similarly "in-place compaction landing" — it's config-gated **default off**; rotation is still the default. Both are one-word fixes.
- **Headline**: "two live tie-ins to work newt landed this very week (#942/#945 … #948 …)" — #945 and #948 are OPEN issues, not landed; only #942 and #950 are merged. The sequencing section gets this right ("#948 already filed"); the headline overstates.

## 3. ACTIONABILITY per ADOPT

- **A1**: no newt file targets (which of `driver.rs`/`transcript.rs`/resume path builds the request?), no invariant checklist, and no eval design — how is KV reuse measured (llama.cpp slot-reuse logs? TTFT deltas on a fixed transcript?). Not startable from the doc alone.
- **A2**: good conceptual plug-ins (LanguagePacks, nudge classifiers) but missing: where the evidence ledger lives on the newt side (observation layer? conversation store?), the stop-gate insertion point in the loop, a flag name, and which eval benchmark shows "premature done" today. Half-startable.
- **A3**: file target given, but see §2 — script runtime, confinement mechanism, tool-subset choice, and caps all undecided. This is a design doc away from startable; the doc should say so.
- **A4**: schema change is clear (active/compacted columns in the conversation store); missing: the recall-side `include_inactive` surface (recall.rs change), flag name, and the retention/redaction decisions (§2). Mostly startable.
- **A5**: startable — compress.rs prompt + marker assembly, fixture-testable as stated. Best-specified ADOPT.
- **A6**: startable as a convention (scheduled/wyvern + systemd timer). Fine.
- **A7**: recall.rs named, but the prerequisite is unstated: newt must first **tag** automation-generated sessions (crew/panel/eval provenance flag in the store) before it can demote them. Without saying where that flag comes from, an implementer stalls at step one. Plus the dropped scan-budget (§1).
- **A8**: 8.2 is a concrete, immediately runnable check (good). 8.1 omits that hermes *scales the aggregate budget to the model's context window* (tool_executor.py:54-60) — a fixed 200K-char constant is wrong for newt's small local models; the scaling rule is the portable part. 8.3 startable.
- **A9**: mechanics well-cited; newt-side targets (crew_tool.rs / scheduler supervision vs #948's new tool) left implicit — acceptable since it's design input to #948, but one line mapping each bullet to "#948 tool" vs "crew supervision" would help.
- **A10**: each item startable; "negative-space taxonomy" should note the experience store is currently in-memory-only (skills report) so the gate lands in experiential.rs write-gate and/or a steer TOML.

## 4. Delta section fairness

Fair and accurate. Every table row checks against sessions-delta and context-compression (v11→v19 at `hermes_state.py:125`; gateway_routing move; 1h freshness gate; tri-state in-place compaction; retired LLM recall; persisted cooldowns; compressor 1,583→3,082). The "new machinery" paragraph correctly surfaces the repair ladder, optimize, origin proofs, display-name neutralization, verification_stop. Reasonable selections omitted: delegate-cascade cycle bug (#49148), session-context subprocess env-leak fix, `/rewind` semantics (sessions-delta itself called rewind "worthwhile *if* newt ever adds undo"), trigram fallback — all defensible. The two default-value glosses noted in §2 ("behind a flag", "landing") are the only fixes needed.

## 5. STRUCTURE

- **A3 is the one arguable misbucket**: by the doc's own sequencing it's "with #948," its cost is the highest in the list, and it carries an unresolved design question (script runtime + confinement). Either move to CONSIDER ("script-RPC — commit after a design pass resolving runtime/confinement, one substrate with #948") or keep in ADOPT with an explicit "requires its own design doc first" marker. Everything else is well-bucketed: A9-as-design-input is honest, background-review is correctly CONSIDER with the reopen-gate justification, AVOID items all carry receipts and none read as disguised CONSIDERs (AVOID-5's "if newt ever supports these families, it's a family-defaults field" is the right escape hatch, kept inside AVOID).
- **Dangling internal reference**: the status line says "see Critique log at the end" — **no critique log exists in the doc**. Either append it or delete the pointer. This is the doc's only broken self-reference.

## 6. Cross-reference verification (all checked at newt-agent)

- `docs/design/evidence/hermes-audit-2026-07/` — EXISTS, contains all 10 reports (report-*.md matching the scratchpad set). Link resolves.
- `context-memory-hermes-learnings.md` and `evidence/hermes-study/` — EXIST.
- progressive-disclosure-compaction gap claim — VERIFIED verbatim at `docs/design/progressive-disclosure-compaction.md:63`: "the compaction marker does not carry the `memory_fetch` handles". A4's use is accurate.
- #942 MERGED "fix(agentic): live-session round-cap grace, command output tails…" — matches "grace-rounds design (#942)". #945 OPEN "run_command output: head+tail cap + searchable spill" — matches. #948 OPEN "subagent tool: single-call sub-agent dispatch" — matches. #950 MERGED "feat(cards): family-level vLLM serving defaults" — matches. Only defect is the "landed this very week" phrasing covering open issues (§2).
- Newt-side plug-in files named in ADOPTs all exist: `newt-core/src/agentic/{tools.rs,recall.rs,compress.rs,spill.rs,scheduled.rs}`; `workflow_grace_rounds` confirmed in driver.rs. The comparison-table "3,400 tests" claim is consistent with ~3,168 `#[test]`/`#[tokio::test]` functions workspace-wide (close enough).

## Prioritized additive fixes

1. Delete or supply the "Critique log at the end" reference (broken self-ref).
2. A4: add one sentence naming the three June-flagged open items (archive retention, secret-redaction of archived rows, verify_chain participation).
3. A3: add the script-runtime/confinement open question + the Footprint-Ladder tension sentence; or rebucket to CONSIDER.
4. Headline: "landed" → "landed or filed" for #945/#948; delta table: "legacy mirror (still written by default) behind a flag"; "in-place compaction landing (default off)".
5. A7: add the 300-row scan-before-dedupe budget and the prerequisite "tag automation-generated sessions in the store first."
6. A8.1: add "scale the aggregate budget to the model's context window (hermes: tool_executor.py:54-60)".
7. A10: add write-hygiene for agent-writable files ([BLOCKED] placeholders, drift guard + .bak) and the two agent-loop trivials (`_turn_exit_reason`, flush-before-destructive-tool).
8. A9: add the "state the child's actual met-down caveat set in its prompt" bullet.
9. A2: add the code-vs-docs path-filter caveat.
10. A1: one clause scoping the delta (newt already has frozen-prefix/frozen-snapshot; the import is byte-identical restore + diagnostics + JSON canonicalization) and an eval-design hint (TTFT/slot-reuse measurement).
11. CONSIDER additions (one line each): typed FailoverReason/TurnRetryState taxonomy; conditional skill-index gating; contribution rubric as steer-TOML data for crew PR review.