# #1526 — main ↔ production reconciliation (pre-B6)

Reconcile the mainline chat-recovery contract (#1533) + behavioral constitution
(#1529/#1530/#1535/#1536) with the #1528 B3/B4/B5 Responses work on
`feat/agent-psyche-production`, on a **dedicated** branch, before B6 formalizes
the combined system. This is the highest-risk merge in the #1528 train; it is
NOT folded into the final #1526 review and NOT resolved on the shared branch.

## Ancestry
| | SHA |
| --- | --- |
| main (parent A) | `26c809e` |
| frozen production (parent B, tag `pre-b6-production-freeze-2026-08-05`) | `54ed418` |
| reconciliation branch | `integration/1526-main-production-reconciliation` (merge of A+B) |

Both parents were verified independently green first (Phase 1):
- main `26c809e`: `..._cw_400_recovery_retries_the_same_logical_round_with_tools` 2/2; `lake build` 11 jobs; `spec/lint-behavior-map.py` 25 contracts OK / 0 errors.
- production `54ed418`: content_spill 21, spill_cid, observation_hook 13 (B4), responses_wire 43 (B5), responses_compaction 19 (B3), memory_fetch 23 — all 0 failed.

## Semantic ownership (the resolution rule)
- **main owns the generic chat-recovery contract** (Ollama + OpenAI-compat Chat Completions loops): context-window / unsupported-tools / malformed-XML recovery each **retry the current logical round in place** and do **not** advance the logical-round counter; retry budgets stay bounded; the tools-disabled summary begins only after *completed* rounds reach the cap. The checked `ChatRecovery` TLA+ model + `BHV-ROUND/RECOVERY` contracts are preserved.
- **production owns the Responses-specific B3/B4/B5 behavior** (see below).
- **shared code satisfies both.** No duplicated recovery loops; no broad refactor during conflict resolution.

## Conflict ledger (3 files)

### 1. `formal/lakefile.toml` — UNION
- **main behavior:** adds the `NewtPolicy` lean_lib (with `NewtPolicy/PromptForm` — the #1536 prompt-contract model) + the constitution defaultTarget.
- **production behavior:** adds `CompactionProvenance`, `CompactionLifecycle`, `CompactionSpill`, `ResponsesUsage`, `ResponsesWire` lean_libs (the #1528 B2/B3/B4/B5 kernels).
- **combined:** `defaultTargets` = the union of all 8; every `[[lean_lib]]` block kept. No lib dropped.
- **proof:** `lake build` builds all 26 jobs (main's `NewtPolicy.Basic`/`PromptForm` + production's five kernels + CaveatLattice/ProjectModel), sorry-free.

### 2. `newt-core/src/lib.rs` — UNION of the `pub use agentic::{…}` re-export list
- **main-only symbols:** `HumanQuestionOutcome`, `PermissionAction` (prompt/action contract).
- **production-only symbols:** `RoundObservation`, `BehaviorSignal`, `parse_context_window_error`, `recover_context_window_400`, spill/wire types, etc.
- **combined:** set-union = **102 symbols**, verified set-equal to the computed union (0 missing, 0 extra). No export dropped.

### 3. `newt-core/src/agentic/mod.rs` — 4 hunks, combine BOTH families
The two sides restructured the same two chat-dispatch loops incompatibly:
- **main (#1533):** wrapped each dispatch in an inner `loop { match dispatch { Ok => break (json, est), Err => … } }` and changed recovery `continue 'round_loop` → bare `continue` so recovery **retries the round in place** (never burns the only tool-capable round at `max_tool_rounds == 1`).
- **production (B3/B4/B5):** kept the richer recovery body (`emit_context_window_400`, `effective_input_ceiling`, `recovered_input_budget`, `compaction_stage`, `apply_to_chat_completions_body`).
- **combined resolution:** main's inner-loop skeleton (retry-in-place) **wrapping** production's full recovery body, per loop. Ollama loop keeps its malformed-XML nudge with a bare `continue`; OpenAI-compat loop keeps `generation_policy.apply_to_chat_completions_body`. Test module = **union** of both sides' tests (production's B5 wire tests + main's cw-400 parity tests/helpers).
- **combined round machine (Phase 5):** only a *completed* model response advances the logical round. Recovery/compaction/retry paths use bare `continue` (retry same round); completed-round paths use `continue 'round_loop` (advance). Verified split: **16** `continue 'round_loop` (advance) vs **14** bare `continue` (retry-in-place); the recovery body markers survive (`emit_context_window_400`×4, `effective_input_ceiling`×18, `compaction_stage`×6, `recovered_input_budget`×7, `apply_to_chat_completions_body`×2).

## Non-regression evidence (Phase 8)
The reconciled tree passes BOTH parents' suites and the full workspace:
- main's recovery parity tests (`openai_/ollama_chat_cw_400_recovery_retries_the_same_logical_round_with_tools`, `..._tools_unsupported_recovers_in_the_same_round`, `..._malformed_xml_retries...`) — pass.
- production's B3 (`content_spill`, `responses_compaction`, `memory_fetch`), B4 (`observation_hook`), B5 (`responses_wire`) — pass.
- full `cargo test --workspace`: 100 binaries, **4495 passed / 0 failed**; `clippy --workspace --all-targets` clean; `lake build` 26 jobs no-`sorry`.

Defect-injection guards remain live (a wrong resolution fails a test): reverting same-round recovery to outer-loop `continue 'round_loop` fails the ChatRecovery parity tests; the #1528 fail-closed tests (CID-commit-before-publish, no-Accepted-on-refusal, no-unvalidated-dispatch) fail if those invariants are broken.

## Out of scope
B6 (the behavioral-conformance proof layer over the reconciled system) is the next slice, on `feat/1528b6-behavioral-conformance` off the reconciled production branch. The final #1526 review is the last step.

## Known flake (not a reconciliation regression)
Under the full parallel `cargo test --workspace`, two timing-sensitive terminal
tests — `tty::spinner::tests::a_cancelled_covered_future_still_erases` and
`::detail_buffers_partial_lines_and_counts_every_char` — intermittently fail.
They pass **isolated + `--test-threads=1`** (6/0), pass on the untouched frozen
production baseline (`54ed418`), and `tty/spinner.rs` is unchanged by this merge;
the byte-identical tree at the reference resolution ran 4495/0. This is the
CLAUDE.md-documented real-resource/timing class that must run single-threaded —
a pre-existing repo flake, orthogonal to this reconciliation. CI re-runs handle it.
