# The next crew lever: four proposals adversarially verified against the #814 autopsy

> Loop-side companion: [`next-loop-levers.md`](next-loop-levers.md) —
> the single-agent loop's decision menu; this doc is the crew side.
>
> Evidence base: `/propose-verify` over
> `scripts/eval/results/autopsy/2026-07-02-pr802-baseline.json` (+ its
> companion `.md`), the root-cause classification of the 17 behavioral
> FAILs in the n=5, 100-row #813 re-sweep (5 models × 2 tasks × 2 modes,
> `scripts/eval/results/sweeps/2026-07-01-pr802-baseline/`), taken
> **after** the #801/#803 planner-decomposition prune had already
> landed and shown no measurable lift on its own (see
> `docs/design/improving-crew-results.md` §7). Companion doc: that same
> file — §§1–6 are the original sweep design, §7–8 are the corrections
> this doc continues from. Every proposal below went through the same
> adversarial-verification discipline that document used in its own §4a:
> **find → try to refute → only what survives is kept.** Rejections are
> kept visible in §4 for the same reason `improving-crew-results.md`
> keeps its own — a rejected fix that looked plausible on the first pass
> is exactly the failure mode this process exists to catch.
>
> House style, inherited from `improving-crew-results.md`: §2 is what the
> evidence **SHOWS** (facts, traced against real code, at HEAD when
> checked). §3 is what is **PROPOSED** (kept, but still hypotheses).
> Every `expected_lift_cells` claim in §3 is a **hypothesis requiring an
> n≥5 `/ab-gate` confirmation**, not a result — the same n=1 lesson that
> bit `improving-crew-results.md` §7 applies here with equal force, and
> is restated per-proposal rather than assumed once.

## 1. What the autopsy SHOWS

The #813 re-sweep (n=5/cell, zero gameable passes, Wilson CIs computed in
code) found crew mode lags single mode on `010-decompose-god-function`
(14/25 `[37%, 73%]` vs single's 25/25 `[87%, 100%]` — non-overlapping,
the one finding that clears the n=5 bar) and is only *suggestive* of a
deficit on `T2-humanize-duration` (19/25 `[57%, 89%]`, overlapping
single's interval — do not cite this as established). The #814 autopsy
classified all 17 FAIL rows from that sweep against the 7-mechanism
taxonomy, with an evidence quote per classification, independently
re-checked:

| mechanism | count | models | tasks |
|---|---|---|---|
| `worker-ignores-scope` | 9/17 | qwen3-coder:30b, qwen2.5-coder:32b, deepseek-r1:70b | both |
| `other` (plausible-but-nonconforming decomposition) | 8/17 | devstral-small-2:24b, qwen2.5-coder:32b, deepseek-r1:70b | both |

Two facts drive everything below:

- **`worker-ignores-scope` is modal and model-agnostic** — spread across
  three of the four models with any failures, on both tasks. The
  cleanest exemplar: qwen3-coder:30b's T2 run `tmp.kFDGwziEa4` — leaf 1
  fixed the bug correctly (crate passing at commit `71d127b`), then leaf
  2, whose entire instruction was "Run the specific failing test to
  verify," rewrote `humanize_duration` into an if/else chain and broke
  the `0 → "0m 0s"` case (tip `f53a3d9`). A verify-only leaf converted a
  PASS into a FAIL.
- **`other` is dominated by structural nonconformance** — nested helper
  functions instead of extracted siblings (all 4 devstral-small-2:24b
  010 runs), `pub` where private was required, `panic!`/`unwrap` bodies
  a whitelist fence rejects. The autopsy's own read: this is *partly*
  planner wording ("at the definition site of `summarize`," read as
  *inside* it) and partly worker interpretation — "split it into
  `worker-nonconforming-structure` vs `planner-ambiguous-leaf` next
  sweep before spending an engineering cycle on it" is the autopsy's own
  stated recommendation, not yet done. Proposal 4 below spends the
  cycle anyway; that tension is called out explicitly in §3.4 and §5.

The autopsy's own "next lever" argument (its §"Next lever," reasons
1–4) is: fix `worker-ignores-scope` first because it's modal and
model-agnostic; the "read-only-verify sub-case is nearly free" and
composes with the existing OCAP caveat machinery; the `other` bucket
needs its taxonomy split before a code fix; and any lift claim needs an
n≥5 re-sweep per the #803 lesson. Proposals 1–2 below are exactly that
argument, made concrete and adversarially checked against real code at
HEAD. Proposals 3–4 are complementary/parallel levers, checked with the
same rigor, not substitutes for it.

## 2. Inertness discipline (why anything survives here at all)

`improving-crew-results.md` §4a's load-bearing insight still governs:
`ratchet.sh` grades the tip of `git branch --list 'crew/*' | tail -1`
against a hidden `grade_spec.rs`; `plan_rc` / exit codes / `run.complete`
are diagnostics the grader never reads. A proposal that only changes a
diagnostic string, a reported completeness flag, or an exit code is
**inert** regardless of how reasonable it sounds. Every proposal in §3
was checked against this oracle by tracing its mechanism through the
real, cited source lines to the point where it either does or does not
change what content gets committed to the `crew/*` tip before any
fail-stop or verify gate runs. One proposal (§4, "grade timeout-killed
runs as a distinct outcome") was rejected on exactly this ground and is
kept visible rather than dropped.

## 3. Proposals PROPOSED (kept, adversarially verified)

Each entry: the problem traced to cited code, the fix, why it clears
the inertness oracle, the concerns raised against it that survived
review, and the reviewer's suggested tightening (`verdict.improvement`)
carried forward as part of the proposal, not as a footnote.

### 3.1 Clamp `fs_write=none` for verify-only leaves in `grant_one_shot_authority`

**Lever slug:** `clamp-verify-only-fs-write` · **files:**
`newt-cli/src/crew.rs` · **risk: low**

**Problem.** `grant_one_shot_authority` (`newt-cli/src/crew.rs:478-488`)
blanket-sets `s.caveat_policy.fs_write = ScopeSpec::default()` ("all")
for **every** subtask, including a terminal leaf whose entire
instruction is to run/verify — the exact `tmp.kFDGwziEa4` exemplar from
§1. `marker_kind`'s existing lexicon-based prune (the #801/#803 fix)
only classifies by the *leading token*, and "Run the specific failing
test to verify..." leads with "Run," which is deliberately absent from
the marker table (the code's own docstring cites "Run the migration" as
real, diff-producing work) — so the leaf survives pruning and is handed
the same unrestricted `fs_write` as a genuine edit leaf.

**Fix.** Add a pure function `is_verify_only_leaf(instruction) -> bool`
that returns true for (a) `marker_kind(instruction).is_some()` (reuses
the #801/#803 classifier unchanged — covers both `Inspect` and mid-plan
kept `Gate` leaves, since kept-to-gate is not kept-to-write) OR (b) a
narrow 3-part AND: leading verb `run`/`execute` (tokenized the same way
`marker_kind` already does) **and** the instruction contains `test`/
`tests` as a word **and** it also contains one of
`verify`/`validate`/`confirm`/`ensure`/`check`. In `grant_one_shot_authority`,
set `fs_write = Keyword(None)` when `is_verify_only_leaf` is true,
leaving `fs_read` granted unconditionally.

**Why it's not inert.** `CrewTask::to_crew_task` computes
`caveats = parent.meet(&self.caveat_policy.to_caveats())`
(`newt-core/src/plan.rs:271-278`) — meet on the `Scope` lattice can only
narrow. `crew_runner.rs:287`'s `if !caveats.permits_fs_write(...)` gate
fail-closes the **entire** "crew" dispatch for a denied leaf *before*
`worktree_id()` / `WorktreeWorkspace::create` (line 346/352) — before
any worktree or `crew/{id}` branch exists, and before `base_ref` could
advance. Since `ratchet.sh` grades the tip of the last `crew/*` branch,
denying leaf 2's authority leaves the tip at leaf 1's already-correct
commit instead of leaf 2's spurious rewrite. This is a change to *what
lands*, not a reporting change.

**Concerns raised (kept).** Condition (a) is largely dead code on the
graded path — `Inspect` leaves are already pruned before this function
runs, so this proposal is carried almost entirely by condition (b).
Condition (b)'s word-boundary matching for the purpose-word half needs
to be tightened to exact-word (not substring) matching before landing,
to avoid a false positive on e.g. "unchecked"/"rechecked" clamping a
real edit leaf. The "leaving `fs_read` granted unconditionally" framing
is moot under the current mechanism (the whole op is refused, so
nothing reads either) — harmless, just an inaccurate justification, not
a bug.

**Constraint alignment.** OCAP meet-only (narrows one leaf's own grant,
never widens; exec/net untouched). Touches zero verify/grading code.
Three-Cs debt flagged explicitly: `RUN_VERBS`/`TEST_OBJECTS`/
`GATE_PURPOSE_WORDS` are new hardcoded lists in the same shape as the
existing `ACTION_MARKERS` table; fold into the in-flight `[plan.prune]`
config surface (#819) once it lands, rather than shipping a second
hardcoded lexicon indefinitely. Fully-mocked unit tier only (in-memory
`Plan`/`Subtask`, no fs/net/subprocess/clock) — 4 tests, including a
regression guard against `marker_kind`'s own stated real-work
counter-examples ("Run the migration," "Run the test suite and fix any
failures").

**Hypothesis cells (n≥5 confirmation required, not claims).**

| cell | claim |
|---|---|
| qwen3-coder:30b × T2-humanize-duration, crew | flip |
| qwen2.5-coder:32b × T2-humanize-duration, crew | harden |
| devstral-small-2:24b × 010-decompose-god-function, crew | no-regression |
| deepseek-r1:70b × 010-decompose-god-function, crew | no-regression |
| qwen2.5-coder:32b × 010-decompose-god-function, crew | no-regression |

### 3.2 Land PR #824 (issue #812) — def-site-derived, meet-only leaf write-lane fence

**Lever slug:** `worker-leaf-scope` · **files:** `newt-cli/src/crew.rs`,
`newt-cli/src/crew_runner.rs`, `newt-core/src/agentic/plan_exec.rs`,
`newt-core/src/plan.rs`, `newt-scheduler/src/crew.rs`,
`newt-scheduler/src/team.rs` · **risk: low** · **status: PR #824 open,
CI green on Rust checks, Windows build+test was in-progress at review
time — land order 1**

**Problem.** Nothing stops an `Edit` at *any* path from landing once a
leaf's `fs_write` is `All`, which every leaf gets today
(`grant_one_shot_authority`, `newt-cli/src/crew.rs:478-488`). Three of
the nine `worker-ignores-scope` items are provably cross-file, not just
same-file overreach — deepseek-r1:70b/010 leaves invent an orphan
`summarize.go`/`main.go`/`path/to/summarize.go` next to `src/lib.rs`
without ever touching it.

**Fix.** This is a lever *already built* — do not hand-roll a second
implementation. `derive_subtask_scope` (`crew.rs`) greps `grep_terms`
against git-grep definition sites to find a leaf's own def file(s),
unioned with a new model-declared `files` field on the plan; this
becomes the leaf's write lane, threaded through `Subtask.context` →
`plan_exec.rs`'s `args["scope"]` → `crew_runner.rs` → a **second**
partition in `newt-scheduler/src/crew.rs::run_crew`,
`scope_permits(scope, path)`, applied **after** the existing
`caveats.permits_fs_write` partition. Effective writable set =
`worktree ∩ fs_write ∩ scope` — meet-only; empty/degenerate scope fails
open to today's behavior. The PR's own adversarial review (17 findings,
all fixed pre-merge) caught and fixed a self-introduced vacuous-pass
hole: a green verify on an unchanged tree with refused edits used to
report `Passed` with nothing landed (worse than doing nothing); it now
retries with the refusal fed back, ending honestly in
`NeedsHumanReview` if attempts exhaust.

**Why it's not inert.** Verified end-to-end: `--one-shot` authoring
populates `Subtask.context` → `plan_exec.rs` forwards it as
`args["scope"]` → `crew_runner.rs::parse_scope_arg` reads it →
`newt-scheduler/src/crew.rs::run_crew` applies `scope_permits` **before**
the commit that lands on the tip branch. A same-PR mock test
(`scope_fence_refuses_over_reach_but_lands_the_in_scope_fix`)
demonstrates the exact flip mechanism directly: an over-reaching
planner has its wrong-file edit refused while its in-scope fix still
lands and passes.

**Honest limit (stated, not hidden).** The fence is **file-granular**
— the same `Scope<String>` exact/prefix-match primitive as `fs_write`
(`newt-core/src/caveats.rs:40-47`), not symbol-granular. It does
**not** fix same-file overreach: two of the facet's own flagship
examples (`tmp.6POCfpztYT`/`tmp.XGdfTylROs` — a "define a private
helper `maximum`" leaf that also silently rewires `summarize`'s call
site, both in `src/lib.rs`; `tmp.kFDGwziEa4`, the §1 exemplar itself,
`validate-fix` rewriting `humanize_duration` in `src/util.rs`) are
same-file and untouched by this PR — only the unenforced
`ONE_STEP_DIRECTIVE` prompt nudge addresses them, probabilistically.
Effectiveness also depends on the planner LLM naming the symbol
(`grep_terms` only sees backtick-quoted or def-adjacent identifiers); a
vaguely-worded leaf falls back to unfenced (safe, non-corrective) —
this limits how many of the 9/17 it can realistically reach, not just
theoretically. `run_team` (team-mode dispatch) is explicitly left
unfenced — filed as follow-up #816; the ratchet exercises
`RosterMode::Crew` only, so this doesn't limit the target cells.

**Constraint alignment.** OCAP: strictly meet-only, second partition
layered on the existing `fs_write` leash, never unioned in. Fixes a
self-discovered vacuous-pass hole into an honest retry path rather than
a new way to fake green. Fully-mocked unit tier throughout.

**Hypothesis cells (n≥5 confirmation required).**

| cell | claim |
|---|---|
| deepseek-r1:70b × 010-decompose-god-function, crew | flip |
| qwen2.5-coder:32b × T2-humanize-duration, crew | harden |
| qwen3-coder:30b × T2-humanize-duration, crew | no-regression |
| qwen2.5-coder:32b × 010-decompose-god-function, crew | no-regression |

### 3.3 PLAN_AUTHOR_SYSTEM: forbid rephrased verify/test subtasks + one-shot minimal-decomposition example

**Lever slug:** `planner-forbid-verify-rephrasing` · **files:**
`newt-cli/src/crew.rs` · **risk: low**

**Problem.** `PLAN_AUTHOR_SYSTEM` (`crew.rs:634-646`) already forbids a
literal-verb blocklist ("Do NOT create separate inspect/understand/
explore/locate/verify/test/run-tests subtasks"), but planners defeat it
by rephrasing. `tmp.zfbIvGkXKX` (qwen2.5-coder:32b, T2) authored 6
dependent leaves for a one-line bug fix, none leading with a forbidden
verb (`test-edge-cases`, `clean-up-code`, `update-comments`, ...); leaf
2 broke scope catastrophically (changed the function signature,
invented `src/main.rs`, deleted existing tests) and the fail-stop
correctly refused to land it — but it also stranded the 4 remaining
leaves. This is the same `planner-over-decomposition` secondary tag on
4/17 items across 3 models (qwen3-coder:30b, qwen2.5-coder:32b ×2,
deepseek-r1:70b), independently re-verified against the real
`.plan.log` and the #814 autopsy artifacts.

**Fix.** Append to `PLAN_AUTHOR_SYSTEM`: one sentence stating the
verb-blocklist prohibition covers rephrasings too, naming the observed
evasions ("add edge-case tests," "clean up the code," "update the
comments," "validate the fix"), plus a worked one-shot example pair
using the exact humanize-duration goal contrasting the correct
single-subtask decomposition against the observed 6-subtask
anti-pattern by name.

**Why it's not inert.** `PLAN_AUTHOR_SYSTEM` feeds `author_plan`'s
`.system()` call (`crew.rs:848`) — it determines what subtasks the LLM
proposes, hence what dispatches and what lands on the tip, upstream of
dispatch/commit entirely (not a downstream reporting change).

**Concerns raised (kept).** The `deepseek-r1:70b × 010` "no-regression"
framing is fine, but a stronger claim (flip) would **not** be
grounded — that cell's 010 failures in the baseline are dominated by
wrong-file/wrong-language edits (orphan Go files) and deleted tests, a
mechanism this prompt sentence does not touch; the proposal correctly
does not claim a flip there. The #814 autopsy's own priority is
`worker-ignores-scope` first (§3.2) precisely because "any fix that
only tunes one model's prompt leaves two-thirds of the bucket
untouched" — this proposal is a legitimate **complementary**, not
substitute, lever and should be sequenced/labeled so any measured lift
isn't misattributed to §3.2. The `qwen2.5-coder:32b × T2` "flip" claim
is optimistic at the whole-cell level: only 2 of that cell's 5 baseline
trials carry the `planner-over-decomposition` tag, so a fully
successful prompt fix would plausibly move a subset of the cell's
failures, not guarantee a full flip. The worked example hardcodes the
literal `humanize_duration` task; risk of overfitting to that specific
example rather than generalizing — worth a second, structurally
different worked example if the eventual A/B shows narrow overfit.

**Constraint alignment.** No caveat/authority change — prompt-only,
shapes what the model proposes, not what it's permitted to do.
Fully-mocked, pure string-containment regression test on the const.

**Hypothesis cells (n≥5 confirmation required).**

| cell | claim |
|---|---|
| qwen2.5-coder:32b × T2-humanize-duration, crew | flip |
| qwen3-coder:30b × T2-humanize-duration, crew | flip |
| deepseek-r1:70b × 010-decompose-god-function, crew | no-regression |

**Sequencing note.** Confirm this lever's lift with its own `/ab-gate`
run restricted to the two T2 cells that actually carry the
`planner-over-decomposition` tag, run **independently** of a §3.2
(#812) merge, so lift (or its absence) isn't confounded between the
two levers.

### 3.4 Add an extraction convention to the crew PLAN-step system prompt

**Lever slug:** `decompose-extraction-convention` · **files:**
`newt-scheduler/src/crew.rs` · **risk: low, but see the sequencing
concern below**

**Problem.** `CREW_PLAN_SYSTEM` — the system prompt handed to the
code-writing worker for every crew leaf
(`ChatRequest::new().system(CREW_PLAN_SYSTEM)`,
`newt-scheduler/src/crew.rs:~401`) — says nothing about *where* a new
function should go, its visibility, or that it must not introduce a new
failure path. This drives most of the `other` bucket's structural
nonconformance: nesting the extracted helper inside the caller (all 4/4
devstral-small-2:24b 010 runs, `git show b670783`), emitting `pub` when
private was required (`tmp.sjHFd5AvP4`), or writing a `panic!`/
`.unwrap()` body a hidden per-helper whitelist rejects
(`tmp.XGdfTylROs`, qwen2.5-coder:32b).

**Fix.** Extract the inline PLAN-step prompt into a documented
`const CREW_PLAN_SYSTEM`, append one convention sentence: define new
helpers as top-level siblings, never nested; exact requested name/
signature; private unless `pub` is explicitly requested; no new
panic/unwrap/failure path the task didn't ask for. The FILE:/END-FILE
emission-format contract is kept byte-identical.

**Why it's not inert.** `CREW_PLAN_SYSTEM`'s output is parsed
(`parse_edits`) and applied via `workspace.apply(&clean)`
(`crew.rs:427`) — the diff that gets committed to the `crew/*` tip.
Confirmed `run_crew` (and this prompt) is the single shared path for
every crew leaf, used by both `crew_runner.rs` and `team.rs`.

**Concerns raised (kept — the strongest of the four).** The
`deepseek-r1:70b × 010` "flip" claim is **not supported** by the cited
evidence. That cell has 5 FAILs in the baseline; only 1/5 (a naming
miss, `fn max` vs `maximum`) is plausibly addressed by this sentence —
the other 4/5 are a different, untouched mechanism: Go files written
into a Rust crate, hand-rolled `Cargo.lock`, a deleted seed test. No
"sibling, exact-name, no-panic" sentence fixes a worker emitting the
wrong programming language into the wrong file. The proposal's own
problem narrative never cites a deepseek-r1 example to justify this
claim; it was added to `expected_lift_cells` ungrounded. The
`qwen2.5-coder:32b × 010` "flip" claim is better grounded but still
imprecise — part of that cell's failure is scope creep (editing
`summarize()` beyond its scope), which is `worker-ignores-scope`
territory (§3.2), not this fix's target. **The #814 autopsy's own
stated recommendation is to split the `other` bucket's taxonomy
(`worker-nonconforming-structure` vs `planner-ambiguous-leaf`) *before*
spending an engineering cycle on it** — this proposal spends the cycle
first. Not a hard violation, just evidence this proposal's priority
diverges from the autopsy's own ordering. The devstral-small-2:24b
claim is the strongest of the three — all 4/4 of that cell's 010 FAILs
are exactly the nested-fn pattern targeted, with matching, verified
line citations.

**Reviewer's required tightening, carried forward as part of this
proposal, not optional:** downgrade `deepseek-r1:70b × 010-decompose-god-function`
from "flip" to "no-regression" (or drop it) before this proposal's
`/ab-gate` run — claiming a flip there overstates evidence the proposal
itself cites and should not survive re-sweep. The devstral and
qwen2.5-coder:32b flip claims stand.

**Constraint alignment.** Pure prompt-string edit; no caveat/authority
change; does not touch `verify_gate.rs`/`ratchet.sh`/`grade_spec.rs`.
Fully-mocked pure string-assertion test.

**Hypothesis cells (n≥5 confirmation required — deepseek-r1 downgraded
per reviewer instruction, see above).**

| cell | claim |
|---|---|
| devstral-small-2:24b × 010-decompose-god-function, crew | flip |
| qwen2.5-coder:32b × 010-decompose-god-function, crew | flip |
| ~~deepseek-r1:70b × 010-decompose-god-function, crew~~ | ~~flip~~ → **no-regression** (downgraded — see concerns) |
| qwen3-coder:30b × T2-humanize-duration, crew | no-regression |
| qwen2.5-coder:32b × T2-humanize-duration, crew | no-regression |

## 4. REJECTED — kept visible (the doc's immune system)

### Tag timeout-killed crew runs with a `killed_by_timeout` diagnostic (no grading change)

**Why rejected — inert on grade.** The only code change is appending
`killed_by_timeout=$killed_by_timeout` to the details string of
`ratchet.sh`'s final `emit` call in the crew branch. That call fires
*after* branch selection (`git branch --list 'crew/*' | tail -1`),
checkout, and the `grade_spec.rs` behavioral test have already run and
already produced `behavioral=PASS|FAIL`. The new field is concatenated
into the details string only; nothing reads it back into anything that
decides which branch is graded or what `cargo test --test grade_spec`
returns. Architecturally identical to the `run.complete` flip rejected
in `improving-crew-results.md` §4a/§4d: a value derived from an
already-final grade, decorating the diagnostic string. The proposal's
own test plan admits this directly ("the behavioral column identical
to the pre-change baseline JSON in all 17 rows"), and its title is
self-admittedly "no grading change."

This rejection is kept here, not deleted, for the same reason
`improving-crew-results.md` keeps its own five: a proposal that reads
as an obvious observability improvement is exactly the shape that
slips past a non-adversarial review. It cost nothing to check and
would have cost a wasted PR to skip checking.

## 5. Prioritized roadmap

All four kept proposals carry `revised_risk: low`, so the ranking below
is driven by **lift ÷ evidence strength**, not risk — the risk
denominator is equal across all four.

1. **`worker-leaf-scope` (§3.2, #812/PR #824) — land first.** Already
   built, CI green on the Rust checks, targets the modal mechanism
   (9/17, model-agnostic, both tasks), and its flip mechanism is
   demonstrated directly in a same-PR mock test, not just argued from
   code-reading. Highest-confidence, largest-addressable lever, and it
   is *shovel-ready* — the only remaining gate is a green Windows CI
   run and the standing `/ab-gate` confirmation this doc's §6 spells
   out.
2. **`clamp-verify-only-fs-write` (§3.1) — land second, after or
   alongside #824.** Composes with it (both are meet-only, layered
   partitions; neither conflicts with the other's mechanism). Targets
   exactly the "read-only-verify sub-case is nearly free" case the
   autopsy's own next-lever argument calls out (its reason 2) — the
   narrowest, cleanest single exemplar of the four proposals, with the
   flip mechanism traced through exact, verified line numbers.
3. **`planner-forbid-verify-rephrasing` (§3.3) — land third, verify
   independently.** Well-grounded (every cited artifact — `.plan.log`,
   autopsy JSON/MD — independently re-checked and confirmed verbatim),
   targets a real but smaller bucket (`planner-over-decomposition`,
   4/17 across 3 models) via a genuinely different mechanism (upstream:
   stop authoring the trap, vs #812's downstream: fence it at
   dispatch). Complementary, not redundant — its `/ab-gate` run must be
   independent of #812's merge so any lift is attributable.
4. **`decompose-extraction-convention` (§3.4) — land last, and only
   after re-scoping.** Targets the `other` bucket (8/17), which the
   autopsy itself says needs a `worker-nonconforming-structure` vs
   `planner-ambiguous-leaf` taxonomy split *before* an engineering
   cycle is spent on it — this proposal spends the cycle first, against
   the autopsy's own stated sequencing. One of its three flip claims
   (deepseek-r1:70b × 010) is unsupported by the evidence it cites and
   must be downgraded to no-regression before its `/ab-gate` run (§3.4
   states this as a required tightening, not an option). The remaining
   two flip claims (devstral, qwen2.5-coder:32b) are solidly grounded
   and worth landing — but consider filing the `other`-bucket taxonomy
   split as its own autopsy-recommended follow-up first, so this
   proposal's real target is at least correctly scoped when it lands.

## 6. Hypothesis discipline (restated, not assumed)

Per the standing n=1 lesson (`improving-crew-results.md` §7 — the
#801/#803 prune's own predicted "30b × T2 flips FAIL → PASS" returned
4/5 vs 4/5 at n=5, and the sweep's headline n=1 FAIL turned out to be
noise at ~80% PASS): **every hypothesis cell in §3 is a claim pending
an n≥5 `/ab-gate` confirmation, not a result.** A mechanism traced
soundly through real code (all four proposals here were) tells you the
lever *can* move a cell — it does not tell you it *does*, at what rate,
or whether local 24B–70B models comply with a prompt-only nudge often
enough to matter. Treat every "flip"/"harden"/"no-regression" label in
this document exactly the way its own field name says: `expected_lift_cells`,
expected, not measured.

## What to run next

For each kept proposal: build its branch, run `scripts/eval/sweep.sh`
to produce a candidate arm at n≥5 over the cells it claims, then run
`/ab-gate` against the existing `2026-07-01-pr802-baseline` arm (the
same baseline the #813/#814 evidence in this doc is drawn from). Do not
reuse a candidate arm across levers — each `/ab-gate` call attributes
lift to exactly one code change, and stacking un-independently-verified
levers into one candidate build reintroduces the confounding §3.3's
sequencing note warns about.

**1. `worker-leaf-scope` (§3.2, land order 1)**

```bash
# Build the candidate binary from branch feat/812-worker-leaf-scope (PR #824), then:
scripts/eval/sweep.sh \
  --out scripts/eval/results/sweeps/2026-07-0X-worker-leaf-scope-candidate \
  --tasks T2-humanize-duration,010-decompose-god-function \
  --modes crew \
  --models deepseek-r1:70b,qwen2.5-coder:32b,qwen3-coder:30b \
  --trials 5
```
```
/ab-gate {"lever": "worker-leaf-scope",
  "baseline_dir": "scripts/eval/results/sweeps/2026-07-01-pr802-baseline",
  "candidate_dir": "scripts/eval/results/sweeps/2026-07-0X-worker-leaf-scope-candidate",
  "expected_flip_cells": ["deepseek-r1:70b x 010-decompose-god-function crew"]}
```

**2. `clamp-verify-only-fs-write` (§3.1, land order 2)**

```bash
# Build the candidate binary with is_verify_only_leaf landed in grant_one_shot_authority, then:
scripts/eval/sweep.sh \
  --out scripts/eval/results/sweeps/2026-07-0X-clamp-verify-only-fs-write-candidate \
  --tasks T2-humanize-duration,010-decompose-god-function \
  --modes crew \
  --models qwen3-coder:30b,qwen2.5-coder:32b,devstral-small-2:24b,deepseek-r1:70b \
  --trials 5
```
```
/ab-gate {"lever": "clamp-verify-only-fs-write",
  "baseline_dir": "scripts/eval/results/sweeps/2026-07-01-pr802-baseline",
  "candidate_dir": "scripts/eval/results/sweeps/2026-07-0X-clamp-verify-only-fs-write-candidate",
  "expected_flip_cells": ["qwen3-coder:30b x T2-humanize-duration crew"]}
```

**3. `planner-forbid-verify-rephrasing` (§3.3, land order 3 — run
independently of #812's merge, per the sequencing note in §3.3/§5)**

```bash
# Build the candidate binary with the PLAN_AUTHOR_SYSTEM rephrasing-prohibition + worked example, then:
scripts/eval/sweep.sh \
  --out scripts/eval/results/sweeps/2026-07-0X-planner-forbid-verify-rephrasing-candidate \
  --tasks T2-humanize-duration \
  --modes crew \
  --models qwen2.5-coder:32b,qwen3-coder:30b \
  --trials 5
```
```
/ab-gate {"lever": "planner-forbid-verify-rephrasing",
  "baseline_dir": "scripts/eval/results/sweeps/2026-07-01-pr802-baseline",
  "candidate_dir": "scripts/eval/results/sweeps/2026-07-0X-planner-forbid-verify-rephrasing-candidate",
  "expected_flip_cells": ["qwen2.5-coder:32b x T2-humanize-duration crew", "qwen3-coder:30b x T2-humanize-duration crew"]}
```

**4. `decompose-extraction-convention` (§3.4, land order 4 — deepseek-r1
downgraded to no-regression per the required tightening in §3.4;
consider filing the `other`-bucket taxonomy split first, per §5)**

```bash
# Build the candidate binary with the CREW_PLAN_SYSTEM extraction-convention sentence, then:
scripts/eval/sweep.sh \
  --out scripts/eval/results/sweeps/2026-07-0X-decompose-extraction-convention-candidate \
  --tasks 010-decompose-god-function,T2-humanize-duration \
  --modes crew \
  --models devstral-small-2:24b,qwen2.5-coder:32b,deepseek-r1:70b,qwen3-coder:30b \
  --trials 5
```
```
/ab-gate {"lever": "decompose-extraction-convention",
  "baseline_dir": "scripts/eval/results/sweeps/2026-07-01-pr802-baseline",
  "candidate_dir": "scripts/eval/results/sweeps/2026-07-0X-decompose-extraction-convention-candidate",
  "expected_flip_cells": ["devstral-small-2:24b x 010-decompose-god-function crew", "qwen2.5-coder:32b x 010-decompose-god-function crew"]}
```
