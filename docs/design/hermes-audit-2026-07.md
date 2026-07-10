# hermes-agent architectural audit — 2026-07

**Status:** proposal, revised after a three-lens adversarial critique (factual /
ledger-fit / completeness — see the Critique log at the end; the critique
rewrote several verdicts, including demoting this doc's original headline
item). **Extends, and is gated by,** the 2026-06-10 study
(`evidence/hermes-study/`, plan:
[`context-memory-hermes-learnings.md`](context-memory-hermes-learnings.md)).
Raw survey evidence for THIS audit:
[`evidence/hermes-audit-2026-07/`](evidence/hermes-audit-2026-07/).

**What was audited:** hermes-agent (NousResearch, MIT) @ `origin/main`
`830165473` (2026-07-06) — note the operator's local checkout tracks a
month-old fork point; upstream moved ~3,886 commits in 30 days past it. Ten
parallel survey agents covered: core loop, sessions/state (delta vs the June
study), memory/learning, skills, tools/RPC, subagents, gateway/deploy,
context/compression, engineering shape, plus a settled-verdicts carrier so
this audit cannot silently re-litigate June's decisions.

## The headline

The June verdict — **"take the algorithms, refuse the architecture"** — is not
just still true; hermes itself spent the month confirming it:

- Hermes **retired LLM-summarized recall**. `session_search` is now "No LLM
  calls anywhere — every shape returns actual messages from the DB"
  (`tools/session_search_tool.py:23-30`). They converged, independently, on
  exactly the FTS5-raw-snippets position newt settled in June.
- Hermes is **walking back session-rotation-on-compaction** — the new
  `compression.in_place` mode (#38763, config-gated, default off) exists
  because rotation spawned an orphaned-child-session bug cluster, which then
  required a cross-process lock table to referee. Newt's stable-session
  design never had the bug.
- Hermes **dropped mid-flight budget-pressure warnings** (per the source
  comment at `agent/agent_init.py:624-629`: "they caused models to 'give up'
  prematurely on complex tasks (#7915)"). Independent evidence for newt's
  grace-rounds design (#942, merged) — do not add "you're running low"
  nudges.
- The velocity picture quantifies the architecture's price: ~3,886 commits /
  ~569 authors in a month, **54% of them `fix`**, on a codebase whose
  god-files (`gateway/run.py` 20.5K lines, `cli.py` 16.2K) force
  subprocess-per-file test isolation and made every lint gate but one
  politically impossible.

So the posture stays: newt does not want hermes's shape. What the month *did*
produce is a set of specific, portable mechanisms — most of them scar tissue
from failures newt will eventually hit too — plus live tie-ins to current
newt work: #942/#950 (merged this week) and #945/#948 (filed, open).

## What actually changed since 2026-06-10 (the delta)

Verified against the June study's claims:

| June claim | Now |
|---|---|
| state.db schema v11 | **v19** — 8 migrations in a month; declarative column-reconciler held up |
| sessions.json = atomic routing index | **Moved into a `gateway_routing` DB table**; sessions.json remains as a legacy mirror (still written by default, disable-able) |
| resume_pending auto-continue | Still there, now **freshness-gated** (default 1h) after zombie-resume bugs |
| Session rotation per compaction | **In-place compaction** built (`messages.active`/`compacted` tri-state; default **off**, rotation still default); archived turns stay FTS-searchable |
| LLM-summary recall mode (PR #20238) | **Retired** — deterministic FTS5 only |
| In-memory compression cooldowns | **Persisted** to columns (survive restart) |
| `context_compressor.py` 1,583 lines | **3,082 lines** — doubled; extraction-from-monolith ongoing |

New machinery worth knowing exists: FTS corruption-repair ladder (rebuild →
dedupe → drop+rebuild-from-canonical, backup first), periodic FTS `optimize`,
resume origin-proofs (a cross-chat `/resume` can no longer hijack another
chat's session), prompt-injection neutralization of chat display names, and a
`verification_stop` evidence gate (below).

## Comparison at a glance

| Axis | hermes | newt | Verdict |
|---|---|---|---|
| Language/shape | Python, god-files (20K-line modules), 635K LOC src + **711K LOC tests** | Rust, small crates, ~3,400 tests, 88% cov | Keep newt's shape — hermes's forces subprocess-per-file test isolation |
| Authority | Denylists, thread-local approval context, fail-open `except: pass` | OCAP caveat lattice, fail-closed, Lean-specced | Keep lattice; hermes's patch history (#33057, #30882) is the case study for why |
| Delegation | `delegate_task`: in-process threads, shared fs, trust-the-summary | crew/team: process + worktree isolation, harness-run verification, commit-to-branch | Keep newt's; steal hermes's *supervision* mechanics |
| Recall | FTS5, no LLM (converged) | FTS5, no LLM (settled June) | Agreement — extend with ranking fixes below |
| Compression | Iterative re-summary, temporal anchoring, in-place rollout | Structural prune → LLM summary → markers, anti-thrash | Steal prompt details below |
| Verification | `verification_stop` evidence ledger (new) | Harness-runs-verification in crew; nothing in single-agent loop | Their newest idea validates newt's oldest law — port it *if measurement shows the gap* |
| Provider quirks | ~200 lines of inline host-sniffing conditionals | model cards + family defaults (#950, merged this week) | Newt's answer is right; hermes shows the counterfactual |
| Eval discipline | None (ship on issue-count evidence) | n≥5 sweeps, Fisher gates, inertness oracle | Nothing to import; every adoption below goes through newt's gate |

---

## ADOPT — recommended, in priority order

Each: mechanism (hermes cite @ `830165473`) → newt plug-in point → payoff →
prerequisite. Everything behavioral goes through the eval gate before
becoming default. *(Numbering reflects post-critique priority; the critique
demoted or cut several items from the original draft — see Critique log.)*

### A1. Output-budget completions (extends #942/#945)
1. **Retrieval-tool threshold pinning — check first, possible live bug.**
   Hermes pins `read_file`'s spill threshold to infinity to prevent
   persist→read→persist loops (`tools/budget_config.py:10-13`). The critique
   verified newt's `budget.rs` has **no `memory_fetch` exemption** — if
   `memory_fetch` output can itself be re-spilled by `tool_offload`, that's a
   live loop bug of the class hermes documented. One check, wave-1.
2. **Per-turn aggregate budget**: if one round's tool results collectively
   exceed a budget, spill the largest — per-result caps miss the
   many-medium-results flood (`tools/tool_result_storage.py:203-254`).
   Portable detail: hermes **scales the aggregate budget to the model's
   context window** (`agent/tool_executor.py:54-60`) — a fixed constant is
   wrong for newt's small local models; the scaling rule is the point.
3. **Streaming head+tail drain** for exec output: 40% head buffer + rolling
   tail deque, memory-bounded *during* the read
   (`tools/code_execution_tool.py:1364-1414`). Newt's `run_command` caps via
   `cap_model_output` around a fully-buffered envelope (verified) — a runaway
   producer is an OOM before the cap. Adopt the streaming shape.
4. **Paging hint**: truncation footers compute the omitted-middle's start
   offset and emit a ready-to-run fetch command (`tools/web_tools.py:536-549`).

### A2. Recall-ranking fixes for the FTS store
**Demote (don't exclude) automation-generated sessions** in ranking —
hermes's cron sessions' repetitive vocabulary dominated BM25 and caused
"recall blindness" (#19434) — **plus the 300-row scan-before-dedupe budget**
that makes demotion sufficient (interactive hits buried under automation
walls still surface), **plus bookends** (first/last 3 messages per hit) so
the model sees goal → match → resolution without paying for transcripts
(`tools/session_search_tool.py:42-58, 756-806`). **Plug-in:** `recall.rs`.
**Prerequisite the doc must be honest about:** newt must first *tag*
automation-generated sessions (crew/panel provenance in the store) before it
can demote them — and the pollution premise needs checking: newt-eval runs
are ephemeral by default (they never enter the store), and whether crew
transcripts land in the operator's recall store is unestablished. Bookends
stand on their own regardless.

### A3. Compression-prompt upgrades (reduced by critique)
1. **Temporal anchoring** (the clean adopt): inject today's date; completed
   actions must be rewritten as dated past-tense facts — "never leave a
   finished action worded as if it still needs doing"
   (`agent/context_compressor.py:1742-1758`). Prevents re-executing
   side-effecting actions (pushes, PR creation) after compaction — a live
   hazard for newt's PR-workflow sessions. Fixture-testable, sweepable.
2. **Summary-of-summary residual**: June's plan (18.5) already adopted
   prev-summary chaining + rehydration; the residual worth taking is the
   prompt detail ("PREVIOUS SUMMARY / NEW TURNS TO INCORPORATE, **continue
   numbering**") and a check that what shipped matches what 18.5 intended.
3. ~~On-pre-compress extraction hook~~ — **cut by critique**: it fires an
   extra LLM call at the exact moment context is full (the most expensive
   possible time on a local model), and June already adopted the cheaper
   end-of-conversation extraction (19.4).

### A4. Subagent supervision mechanics (design input to #948 / crew)
Hermes's `delegate_task` is, in effect, #948 already built — with the wrong
isolation (threads + shared fs; see AVOID) — so take only the
**architecture-independent supervision subset** (the critique cut the
summary-cap and background-capacity machinery, which presume the
trust-the-summary/async model AVOID rejects):
- **Progress-based staleness, not wall-clock timeouts**: heartbeat carries
  (current tool, iteration); different stale thresholds for idle vs in-tool;
  staleness stops the heartbeat so the supervisor reaps
  (`tools/delegate_tool.py:425-441, 1767-1834`). Same philosophy as newt's
  grace rounds, one level up — apply to crew role supervision.
- **Structured `tool_trace` + cost rollup** in the return payload: per-call
  {tool, arg/result bytes, ok} skeleton + child tokens folded into parent
  accounting (`:2059-2142`) — auditable without ingesting transcripts; feeds
  newt's eval graders.
- **Completion carries the dispatch spec verbatim** ("the parent may be deep
  in unrelated context and won't remember why the subagent existed"), and
  re-enters as a fresh message, never spliced mid-turn
  (`tools/async_delegation.py:9-29`).
- **0-API-call failure diagnostics** with a stack/state dump (`:1949-1975`)
  — when a child dies before its first model call, say what it was doing.
- **State the child's actual met-down caveat set in its prompt**, computed
  from config, so it doesn't confabulate capabilities it lacks
  (hermes grounds its depth/nesting notes the same way, `:672-735`).

### A5. Engineering hygiene (cheap, near-verbatim)
- **Guard canary self-tests**: a test file that *attacks* each safety guard
  and asserts refusal, so weakening the guard is itself a red test
  (`tests/test_live_system_guard_self_test.py` — written after tests killed
  the developer's live gateway 5+ times in 3 days). Newt version: canaries
  that attempt real fs/exec/net escapes through the mock layer and through a
  floor-level caveat set.
- **Merge-base history check** in CI (~10 lines; written after an
  orphan-history merge collapsed ~1,500 files of blame).
- **Footprint Ladder** as a `docs/decisions/` entry: ordered placement rule
  for new capability (data file → steer TOML → crew role → MCP → new core
  tool last), justified by per-call schema cost. *(Applied below: it is the
  reason script-RPC moved to CONSIDER.)*
- **Write hygiene for agent-writable files** (NOTES.md is a live write
  path): threat-scan at write and at snapshot load with visible `[BLOCKED]`
  placeholders; round-trip drift guard + `.bak` before any full-file rewrite
  — prevents silent clobbering of the operator's hand edits
  (`tools/memory_tool.py:78-241, 704-757`).
- **`_turn_exit_reason` on every loop exit path** (feeds eval autopsy
  directly) and **flush-transcript-before-destructive-tool** (a tool that
  kills the process leaves a resumable transcript) — both trivial
  (`agent/conversation_loop.py:4644-4656`).
- **Negative-space capture taxonomy** as a data file: reject
  environment-dependent failures, negative tool claims ("X is broken" hardens
  "into refusals the agent cites against itself for months"), transients,
  one-off narratives (`agent/background_review.py:250-269`). Note: newt's
  experience store is embryonic (in-memory), so this lands in the
  experiential write-gate and/or a steer TOML.
- **Persist compression cooldowns** and the **FTS `optimize` +
  rebuild-from-canonical repair ladder** — two store-hardening items from
  hermes's month of regressions. *(Two draft items cut by critique: rowid-
  vs-timestamp ordering — newt's §6 law already forbids timestamp ordering;
  freshness-gating auto-resume — newt's resume is passive, there is no
  auto-continue to zombie.)*

### A6. Prefix-cache invariant — audit + enforcement test (demoted by critique)
The June ledger already settled that frozen-prefix discipline "is already in
the trait contract" and frozen-snapshot memory shipped — so this is **not an
import, it's an audit** of an invariant newt claims to hold, plus the narrow
deltas newt lacks: (a) **byte-identical system-prompt persist/restore on
resume**, with logged diagnostics for every miss cause
(`agent/conversation_loop.py:277-398`); (b) an **audit that the
prune/compression pipeline never rewrites the retained prefix** between
compactions (each compaction legitimately busts the cache; the invariant is
about everything in between); (c) ephemeral blocks only in the API-time copy
of the current user message. Skip hermes's tool-call JSON canonicalization —
that fixes Python dict-order nondeterminism serde doesn't have. **Gate:**
define the measurement first (llama.cpp slot-reuse logs / TTFT deltas on a
fixed transcript) — without a cache metric this can't clear the eval gate,
and hermes's own "~75% cost reduction" is a docstring claim, not a
measurement.

### A7. Verify-on-stop — measurement first, then maybe the mechanism
Hermes's newest subsystem is newt's oldest law arriving late: a passive
SQLite **evidence ledger** (terminal tool classifies commands against the
project's verify commands, records pass/fail; file tools mark "workspace
edited"), and a policy-only stop gate that refuses a final answer when code
was edited without fresh passing evidence — one bounded (≤2) nudge quoting
the last failing output, both synthetic messages stripped from persistence
(`agent/verification_evidence.py:383-428`, `agent/verification_stop.py`,
`agent/conversation_loop.py:5110-5155`). Implementation caveats that matter:
the code-vs-docs path filter (prose edits must not trigger it), and the
strip-from-persistence rule (#55733 — resume adjacency + prefix poisoning).
**But the critique's inertness challenge stands:** newt has no measured
premature-done incidence in the solo loop, and a stop-time refusal is a
budget-pressure signal of the same family hermes removed mid-flight (#7915)
— untested on weak local models, and actively worse when the real failure is
round exhaustion. **Adopt order: build the premature-done metric into the
eval harness first; port the mechanism only if the metric shows the gap.**
Newt-side pieces when it does: LanguagePacks (verify commands as data), the
nudge classifiers, the observation layer for the ledger.

---

## CONSIDER — worth a design pass, not yet a commitment

- **Script-RPC tool (`execute_code` analog) — moved from ADOPT by the
  critique's security analysis; requires its own design doc.** The mechanism
  is real and valuable: model submits a script; a generated typed stub module
  exposes the session's tool subset; a child process calls tools back over a
  socket, each dispatched through the normal pipeline; only stdout returns
  (`tools/code_execution_tool.py:62-70, 541-571, 1849-1868`). It collapses
  grep/read/filter pipelines into one round — the most direct relief for the
  round-budget pressure observed live this week. **But under newt's OCAP
  model, "the dispatcher takes the met-down caveat set" secures only the RPC
  leg, not the execution leg:** (1) a script body is a Turing-complete argv
  bypass — inside the child, `open()`/`socket()`/`exec()` are syscalls no
  caveat sees; hermes's confinement is a Unix socket and env-scrubbing
  (the trust model AVOID-4 exists to reject); (2) a snapshotted caveat set
  crossing a process boundary re-decouples authority from execution — the
  same bug shape as hermes's #33057/#30882; (3) interactive approval caveats
  can't fire from inside a running script, and **wyvern has no human and no
  hatch** — every script-triggered caveat must be lattice-decidable without
  a prompt; (4) aggregation defeats per-call caveats: ten thousand
  individually-permitted reads composed at machine speed, funneled through
  stdout — the one channel no caveat governs. **Prerequisites:** a
  script-execution caveat class (interpreter admission, wall/CPU limits,
  syscall confinement, no-net-in-child default, aggregate stdout budget,
  call-count budget, snapshot invalidation on attenuation), OS-level child
  confinement, and the interpreter-footprint decision (what does a Rust
  local-first binary run — embedded interpreter? system Python?) run through
  the Footprint Ladder. Sequence alongside/after the captured-shell OCAP
  work; one substrate with #948, per AVOID-12. *(Avoid their round-refund
  for script calls — it makes budgets unanalyzable.)*
- **ExecBackend seam** (`tools/environments/base.py`): abstract execution to
  "run this bash string, return output+exit," with state reconstructed via
  env-snapshot re-sourcing and an in-band CWD marker — how local, SSH,
  Docker, and Modal become interchangeable behind one wait/interrupt loop. A
  Rust `trait ExecBackend` would serve the DGX/homelab workflow (remote exec
  without a second deployment; OCAP attenuates the *script*, orthogonal to
  *where* it runs). Port the seam **and its edge-case tests**
  (grandchild-holds-pipe hang, torn snapshot writes, orphan process groups);
  do NOT port the trust model (they re-source captured shell functions —
  their snapshot is convenience, not a boundary). Same OCAP wave as
  script-RPC.
- **Post-task background self-review** (memory/skill sediment): June
  rejected hermes's fork mechanism on *economics* ("Anthropic-hosted-cache
  shaped, not Ollama-shaped") — and, per the critique, **that economics
  prong is unchanged**: a caveat-confined crew job still pays full local
  inference for the review pass. What has changed is available mechanism
  (crew, #948) — a vehicle, not a constraint — so reopening this is a
  **maintainer-discretion call, not a gate-rule pass**. If pursued: git-init
  the skills dir (per-edit versioning/rollback for free — strictly better
  than hermes's tarball snapshots), keep "nothing to save" the default
  (hermes's "Be ACTIVE" bias manufactured the sediment its 4,300-line
  curator exists to garbage-collect), adopt their patch-first preference
  order as steer text, and gate on an n≥5 sweep.
- **Steer channel** (mid-turn, non-interrupting redirect): stash under lock,
  drain into the last tool-result message pre-API-call, static system-prompt
  note announcing the channel (`run_agent.py:2720-2770`). Plain-scroller
  compatible (a queue, not a widget). Worth it when interactive sessions are
  long enough that interrupt-and-restart hurts.
- **Empty/thinking-only recovery ladder**: partial-stream recovery → nudge →
  prefill continuation → bounded retries → provider fallback, every synthetic
  message flagged and stripped before persistence
  (`conversation_loop.py:4768-5013`). Newt's stall classifiers detect these
  states; the ladder adds recovery *order* and transcript hygiene. Local
  models are exactly the population that goes empty after tool results.
  Pairs with a **typed error taxonomy** (`FailoverReason` enum → recovery
  action; `TurnRetryState` one-shot guards, `agent/error_classifier.py:24-73`)
  — cleaner in Rust than in hermes.
- **Always-stream for liveness**: hermes streams even with no consumer
  because it enables 90s stale-stream detection — subagents otherwise hang
  forever on SSE keep-alive pings (`conversation_loop.py:1252-1262`).
  Relevant to overnight sweeps and crew workers.
- **Soft-archive compaction — demoted from ADOPT; try the 80%-built fix
  first.** Hermes keeps compacted turns FTS-searchable via an
  `active`/`compacted` tri-state (`hermes_state.py:3651-3701, 4521-4527`),
  closing "compression amnesia." But newt's
  `progressive-disclosure-compaction.md` already identifies the narrower
  fix: **carry the `memory_fetch` spill handles in the compaction marker** —
  newt already spills *redacted* verbatims to an addressable store; the
  marker just doesn't hand over the keys. That closes the same gap with
  redaction preserved and no schema change. Escalate to tri-state only if
  handle-carrying proves insufficient — and then only with the June-flagged
  riders the ledger attached: archive retention policy, a **secret-retention
  adversarial review** (raw archived turns would be recallable forever —
  redaction becomes theater without redact-on-archive), `verify_chain`
  participation of archived rows, and A2-style demotion of archived rows in
  ranking (otherwise the archive floods BM25 exactly the way hermes's cron
  sessions did).
- **Wake-gate convention** (demoted from ADOPT — newt has no cron subsystem;
  the scheduler is parked by decision): hermes lets a $0 pre-run script's
  last stdout line `{"wakeAgent": false}` skip the LLM turn entirely
  (`cron/scheduler.py:2041-2065`). For newt this is an *operator convention*
  (systemd timer runs a probe script; invokes newt only on signal) worth one
  paragraph in the mesh/scheduled design docs when those wake — zero lines
  of newt code today.
- **Delivery-mirroring** — anything pushed to the operator out-of-band gets
  appended to the conversation store so the next session can recall it
  (`gateway/mirror.py`). **Ledger note:** June assigned cross-device
  continuity to mesh (Phase 16); this is store-side continuity machinery and
  belongs to that phase, not before it.
- **Context breakdown observability**: per-category token attribution
  (system/tools/skills/conversation) aligned to the compressor's own
  estimator (`agent/context_breakdown.py`) — a `/context stats` extension.
- **Task-keyed filesystem snapshots** for hibernate/resume of long-lived
  remote work (their Modal snapshot store; newt analog: tar/overlayfs of a
  worktree keyed by task).
- **Never advertise the unavailable**: rebuild tool schemas so descriptions
  only reference tools that passed availability (`model_tools.py:453-513`) —
  an anti-phantom-reach lever; and **check_fn flake grace** (serve
  last-good-within-60s on probe timeout) if newt ever gates tools on MCP
  liveness probes.
- **Conditional skill-index gating**: `requires_tools`/`platforms`
  frontmatter + posture-based names-only demotion
  (`agent/skill_utils.py:163-304`, `prompt_builder.py:1599-1619`) — cheap
  token control for a growing `~/.newt/skills` index, independent of any
  write path.
- **Contribution rubric as steer-TOML data**: hermes's dual-audience
  AGENTS.md encodes machine-actionable accept/close criteria with explicit
  when-NOT-to-close rules (`AGENTS.md:29-42`) — directly reusable shape for
  crew PR-review roles.
- **Content-based sweep resume** for the eval harness: identify completed
  work by (task, seed, binary-hash) content key scanned from outputs, not by
  checkpoint index (`batch_runner.py:821-853`).

## AVOID — deliberately not replicating (with the receipts)

1. **The monolith.** 20.5K-line `gateway/run.py`; a single ~4,700-line
   turn-loop function; test isolation only achievable by spawning a fresh
   interpreter per test *file*; lint gates reduced to ONE blocking rule
   ("intentionally disabled while we wrangle typechecks"); no coverage gate;
   54% of a month's ~3,886 commits are `fix`. Hermes can pay this because
   ~569 contributors grep one file; newt is one operator whose leverage *is*
   the small-crate + zero-warnings + coverage-ratchet discipline.
2. **In-process, shared-filesystem subagents.** Their `FileStateRegistry`,
   sibling-write reminders, global toolset save/restore dances, and
   approval-callback thread-local deadlock fixes are all patches for problems
   newt's worktree-per-role model makes unrepresentable. Also: threads can't
   be killed — "interrupt does not hard-kill the worker thread (Python
   can't)". Keep processes + worktrees for #948.
3. **Trust-the-summary delegation.** A hermes child is "completed" because a
   non-empty summary exists. No harness verification, no commit gate. This
   violates newt's patch-not-prose law directly; crew's
   harness-runs-verification + commit-to-branch is strictly stronger.
4. **Denylist/thread-local authority.** `DELEGATE_BLOCKED_TOOLS` frozensets
   kept "in lockstep" by hand; approval context in ContextVars that silently
   vanished across threads and auto-approved dangerous commands
   (#33057/#30882); secret-substring env scrubbing with a documented
   false-negative history. The caveat lattice is the answer; hermes is the
   maintenance-tax demonstration for the wrong one.
5. **Inline provider-quirk matrices.** ~200 lines of host-sniffed
   reasoning-echo reconciliation (echo required by DeepSeek/Kimi,
   422-rejected by Mistral/Groq, space-pad magic for DeepSeek V4) hardcoded
   in helpers. Newt's model cards + family defaults (#950) are the three-Cs
   answer — if newt ever supports these families, `reasoning_echo:
   require|forbid` is a **family-defaults field**, not code.
6. **Fail-open `except Exception: pass` as posture.** Pervasive; rational for
   a chat product, incompatible with fail-closed OCAP.
7. **Hosted/vendor tiers.** Honcho dialectic user-modeling (every turn ships
   off-box, uncapped injection, unmeasured value), eight interchangeable
   memory-provider plugins, managed Modal via a vendor gateway, Fly
   scale-to-zero. All anti-local-first; the June rejections stand unchanged.
8. **Platform breadth.** ~25 chat adapters whose quirks leak into shared
   helpers. The operator needs one reliable nudge channel; breadth is
   hermes's telescope, not newt's.
9. **Write-biased self-improvement.** "Be ACTIVE — most sessions produce at
   least one skill update… a pass that does nothing is a missed learning
   opportunity" manufactured sediment that then required a 4,300-line
   curator/backup subsystem — whose LLM consolidation pass shipped OFF after
   it archived live skill clusters. Newt's write-gate philosophy
   (high-signal or nothing, eval-gated) is correct.
10. **Accreted lifecycle booleans.** `suspended`/`resume_pending`/
    `was_auto_reset`/`is_fresh_reset`/`expiry_finalized`/`active`/`compacted`/
    `rewind_count` across two stores, with a visible fraction of the month's
    commits fixing their interactions. Session lifecycle should be a small
    typed state machine — Rust makes this nearly free.
11. **Unbounded multiplicative knobs** (`max_concurrent_children`,
    `max_spawn_depth`: floors, "no ceiling", one log warning) and **budget
    refunds** for favored tools — cost becomes exponential and unanalyzable.
12. **Three parallel orchestration stacks** (delegate_task, kanban
    dispatcher, research runners) with different isolation and no shared
    authority model. Newt keeps crew/team/#948 on one substrate.

## Settled verdicts — checked, mostly unchanged

Per the June decision ledger (gate rule: a rejection reopens only on material
hermes change *plus* a newt-side constraint change): embeddings-for-recall
(hermes converged toward newt), exact tokenizers (hermes still ships
chars/4), the 10-step provider waterfall (still their shape; newt probes),
dual-write JSONL+SQLite (they're actively paying for it), gateway session
keys/handoff (unchanged), timestamp MRU (binding mesh law), Anthropic cache
breakpoints (N/A — and note A6 above is the *other half* of that ledger
entry: the frozen-prefix discipline newt already holds; A6 is scoped as an
audit of it, not a re-import), MEMORY/USER split (single-user; unchanged),
13-section template, `max_iterations=9999` curator fork. The
background-review reopen in CONSIDER is explicitly labeled
maintainer-discretion — its June economics rejection is *not* overturned.

## Suggested sequencing

1. **Now / cheap** (each its own small PR): A1.1 threshold-pinning check
   (possible live bug — first); A5 hygiene items; A3.1 temporal anchoring;
   A2 bookends (the ranking demotion waits on automation-session tagging).
2. **Next** (eval-gated): A1.2/1.3/1.4 output budgets; the
   progressive-disclosure marker-handles fix (the 80%-built compression-
   amnesia answer); **the premature-done metric** (A7's prerequisite) and
   **a cache metric** (A6's prerequisite) added to the eval harness.
3. **With #948** (already filed): A4 supervision mechanics fold into that
   design.
4. **OCAP wave** (after captured-shell/enforcement-floor work): script-RPC
   caveat-class design doc + ExecBackend seam — one substrate with #948.
5. **Conditional / later**: verify-on-stop (if the metric shows the gap);
   prefix-cache audit (if the cache metric justifies it); soft-archive
   tri-state (only if marker-handles proves insufficient, with the security
   riders); background review as crew job (maintainer-discretion, behind a
   sweep); steer channel; mirroring (with mesh).

Every behavioral adoption goes through the standing discipline: propose →
inertness check → implement behind a flag → n≥5 sweep → default only on
measured lift.

## Critique log

Three independent critique agents reviewed the first draft (factual
spot-check against hermes source; settled-verdict/design-law fit;
completeness vs the evidence base). Material outcomes folded into this
revision:

- **Factual:** all load-bearing hermes claims verified TRUE against source
  (velocity recomputed: 3,886 commits exact, 54% fix exact, authors ~569 not
  566; schema v19 confirmed; wake-gate, verify-on-stop, execute_code, and
  delegate supervision mechanisms all as described). Fixed: an
  over-attributed quotation, "removed" → past-tense-comment phrasing for
  #7915, "landed this week" → merged-vs-filed accuracy, two delta-table
  default-value glosses.
- **Fit (the heavy one):** original headline item (prefix-cache "single
  highest-value import") **collided with June ledger §3.11** — newt already
  holds frozen-prefix discipline; reshaped to an audit (A6) with a
  measurement prerequisite. Original verify-on-stop ADOPT reshaped to
  measurement-first (A7) — no evidence newt's solo loop exhibits
  premature-done, and stop-refusal is kin to the pressure-nudges hermes
  removed. **Script-RPC demoted to CONSIDER** on a four-part OCAP security
  case (script body = argv bypass; caveat-snapshot TOCTOU; approval
  semantics headless; aggregation defeats per-call caveats) — it needs its
  own caveat class before it can exist. **Soft-archive demoted**: the
  80%-built marker-handles fix answers the same gap with redaction
  preserved; tri-state only as escalation, with the ledger's
  secret-retention riders. **Cron wake gate cut** from ADOPT (newt has no
  scheduler; it's parked — reduced to a convention note). Background-review
  reopen re-labeled maintainer-discretion (the June *economics* rejection is
  unchanged; only the vehicle is new). Subagent supervision cut to the
  architecture-independent subset. Two inert hygiene items dropped (rowid
  ordering — §6 already forbids timestamp ordering; resume freshness-gating
  — newt's resume is passive).
- **Completeness:** added write-hygiene for agent-writable files,
  `_turn_exit_reason` + flush-before-destructive, the 300-row scan budget
  and tagging prerequisite to recall ranking, the window-scaling rule to the
  aggregate budget, met-down-caveats-in-child-prompt to A4, the typed error
  taxonomy / skill-index gating / contribution-rubric CONSIDER lines, and
  the code-vs-docs filter caveat to verify-on-stop.
