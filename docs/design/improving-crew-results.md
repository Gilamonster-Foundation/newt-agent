# Improving the Crew: from a coin-flip to a ratchet

> Evidence: an autonomous DGX planner-strength sweep (2026-07-01) driving
> `scripts/eval/ratchet.sh` crew mode against local models on a DGX host —
> model **names** only, no host/endpoint appears here. n=1 per cell (see §6,
> "Multi-trial confirmation"). Companion issues: #800 (fail-stop), #801
> (planner over-decomposition). The proposals below were adversarially verified;
> five tempting fixes were rejected as inert against the actual grade — that
> analysis is kept in §4 rather than hidden.

## 1. TL;DR

Sweeping the planner==navigator==triage model from 14b up to r1:70b across two
tasks, crew-mode success is **non-monotonic in model strength** — devstral24
PASSes T2, a stronger 32b FAILs it, an even stronger r1:70b PASSes it again —
while single-agent mode passes 8/8 of the same task×model cells, so every
single-vs-crew gap isolates the *harness*, not the model. The dominant kill
switch is **nothing-to-land**: a leaf that verifies but produces no diff (a prior
leaf already did the work, or the planner authored a read-only "inspect" /
trailing "run-tests" leaf) is returned as `Err`, and `run_plan_with_reground`
hard-`break`s on the first `Err`
(`newt-core/src/agentic/plan_exec.rs:141-144`). The fix is **not** the tempting
one (make the fail-stop fail-soft — that is inert because the ratchet grades the
tree, not the exit code); it is to stop *producing* the trap upstream: fence each
per-leaf worker to its declared file scope so work lands in its own lane
(worker-grounding, #800-adjacent) and deterministically prune the non-actionable
leaves that feed the trap (planner-decomposition, #801).

## 2. What the sweep SHOWED

Instrument: `scripts/eval/ratchet.sh`, crew mode =
`newt plan --goal "<t>" --one-shot --dir <throwaway> --max-leaves N`, graded by
an **ungameable** external `grade_spec.rs` (dropped into the produced
`crew/* | tail -1` branch only at grading time; the agent never sees it). Single
mode = `newt-eval run`. Model swept homogeneously (planner == navigator ==
triage) across two tasks:

- **T2-humanize-duration** — a ~1-line fix. Single mode passes for *all* models.
  HAS a hidden `grade_spec.rs`, so the crew grade is ungameable.
- **010-decompose-god-function** — extract 3 private helpers; MUST decompose
  (>=3 leaves). NO hidden `grade_spec`; only a seed behavioural test ships, so a
  green seed is `PASS?gameable` (a false positive for the refactor goal).

Single mode passes 8/8 the same task+model cells, so **every single-vs-crew gap
below is a property of the harness.**

| Model | T2 (humanize-duration, ungameable) | 010 (decompose-god-fn, no hidden spec) |
|-------|------------------------------------|----------------------------------------|
| **14b** | 1 leaf → worker NEEDS-HUMAN-REVIEW (can't code the fix in 3 tries) → **FAIL** | 6 leaves → nothing-to-land; seed passes (`PASS?gameable`) |
| **devstral24** | 3 leaves → leaf-1 "inspect" ACCIDENTALLY writes the whole fix → **PASS** (by accident); leaf-2 nothing-to-land | 4 leaves → complete; seed passes |
| **30b** | 3 leaves → nothing-to-land at an intermediate leaf → **FAIL** (nothing lands) | 4 leaves → nothing-to-land; seed passes |
| **32b** | 1 leaf → worker NEEDS-HUMAN-REVIEW + spurious `Cargo.toml` edit → **FAIL** | 6 leaves → nothing-to-land |
| **r1:70b** | 1 leaf → clean → **PASS** | 3 leaves (must-decompose contract met) |

*(A sixth model, `nemotron-3-super:120b`, was swept but is **excluded**: its
llama-server did not finish loading within the request timeout — Ollama 500,
"timed out waiting for llama-server to start" — so no inference ran. An ops note
about pre-warming large models, not a crew result.)*

Three observations carry the section:

- **Non-monotonic vs strength.** On T2: devstral24 PASS, 32b FAIL, r1:70b PASS.
  Capability does not predict crew success — this is a coin-flip, not a ratchet.
  (Single mode is monotone-clean: 8/8. The variance is entirely in the
  decomposition machinery.)
- **"One leaf lands the whole fix."** devstral24's read-only *inspect* leaf on T2
  wrote the entire fix. Decomposition is not *executed* as decomposed: each
  per-leaf worker holds `fs_write` over the worktree plus the whole goal, and
  tends to implement the entire change in whatever leaf it is on. The PASS is
  therefore an accident, and it is the same mechanism that manufactures
  `EXPERIMENT.md`'s orphan/vacuum files when the leaf text is abstract.
- **Nothing-to-land is the #1 kill switch.** It is the trigger in 6+ cells above
  (both 010 columns, 30b×T2, devstral24's leaf-2). A leaf that verifies but
  produces no diff is returned as `Err`, and the executor fail-stops on it.

## 3. Root causes (the 7 mechanisms)

1. **Fail-stop.** `run_plan_with_reground`
   (`newt-core/src/agentic/plan_exec.rs:141-144`) marks the leaf `Failed` and
   hard-`break`s on the FIRST leaf that returns `Err`; remaining pending leaves
   never dispatch, and `newt plan` exits `i32::from(!run.complete)` = 1
   (`newt-cli/src/crew.rs:626`). Filed as **#800**.
2. **Nothing-to-land → `Err`.** A leaf that VERIFIES but produces no diff (a prior
   leaf already did the work, or it is a no-op "validate" leaf) is reported as
   `Err` via `crew_dispatch_result(..., did_land=false)`
   (`newt-cli/src/crew_runner.rs:240-245`; the "no changes to land" path around
   `commit_to_branch`, `crew_runner.rs:398`) → trips the fail-stop. The #1
   observed trigger.
3. **Per-leaf verify asserts END-STATE on INTERMEDIATE leaves.** The per-leaf gate
   runs the task's full behavioural test; a non-final leaf cannot make it pass →
   `Err` → fail-stop (30b T2 died at an "inspect" leaf).
4. **Worker ignores leaf scope.** `run_crew` (`newt-scheduler/src/crew.rs:243`)
   frames the worker as "a senior engineer implementing a change"
   (`crew.rs:365`) over `"TASK:\n{task}\n\nRELEVANT FILES:\n{curated}"`
   (`crew.rs:378`) with NO signal that this is ONE leaf of a plan and NO file
   fence — the navigator discovers files freely (`crew.rs:280-285`) and the apply
   partition refuses only on `permits_fs_write` (`crew.rs:416-418`), which is
   `Scope::All` on the plan path (`grant_one_shot_authority`,
   `newt-cli/src/crew.rs:478`). A capable model expands "inspect summarize" into
   "rewrite summarize" and edits whatever it likes. Same mechanism produces
   `EXPERIMENT.md`'s vacuum/orphan files.
5. **Worker spurious edits + weak self-correction.** 32b touched `Cargo.toml` on a
   pure-logic fix and failed all 3 retry attempts.
6. **Planner over-decomposes.** It emits non-actionable read-only
   "inspect/examine" leaves and trailing "validate-build/run-tests" leaves that
   produce no diff → feed the nothing-to-land trap; it over-decomposes 1-line
   tasks (30b: 3 leaves for a one-liner). `PLAN_AUTHOR_SYSTEM` (`crew.rs:634`)
   already forbids this in prose, but weaker planners ignore prose and there is no
   deterministic backstop. Filed as **#801**.
7. **Grading integrity.** 010 has no hidden `grade_spec` ⇒ `PASS?gameable`; its
   seed behavioural test is a FALSE POSITIVE for the refactor goal (helpers
   extracted but `summarize` not rewritten ⇒ behaviour unchanged ⇒ test green).
   The exit code is a FALSE NEGATIVE (says incomplete on code that passes).
   Neither signal is trustworthy alone.

## 4. The proposals

### 4a. Executor-semantics fixes — tempting, but mostly INERT on the sweep grade

The natural first instinct is to repair the fail-stop chain (root causes #1–#3).
We checked each against how the ratchet actually grades, and **the exit code is
not the verdict**: `ratchet.sh` selects the tree via
`git branch --list 'crew/*' | tail -1`, drops `grade_spec.rs` in, and runs it;
`plan_rc` / `run.complete` (`newt-cli/src/crew.rs:626`) is only an emitted
diagnostic (results/ratchet-findings.md shows a T2 crew with `plan_rc=0` *and*
`behavioral=FAIL` — the two are independent). So:

- **fail-soft (exit 1 → 3).** REJECTED for the sweep. The grade ignores the exit
  code, and landed work already persists — `commit_to_branch`
  (`crew_runner.rs:398`) writes `crew/{id}` to the shared object store BEFORE the
  `break`, and the grader reads it directly. Changing the exit code moves zero
  cells. (The premise "exit 1 discards landed work" is false.)
- **nothing-to-land → flip `plan_rc`.** REJECTED. Decoupled grade again: for the
  canonical case (fix landed, trailing no-op fails) the grade ALREADY passes,
  because each leaf forks off the chain tip and `tail -1` is the branch on the fix
  tip. Flipping `plan_rc` changes no PASS/FAIL cell.
- **leaf-verify (compile-gate instead of behavioural per-leaf gate).** REJECTED.
  Misattributed: 30b T2 died on an *inspect* leaf that produced NO diff →
  "no changes to land" → `Err` → fail-stop (root cause #2), NOT a
  behavioural-gate false-fail (#3). A compile-gate cannot turn a no-diff leaf into
  a landing leaf, so 30b T2 stays FAIL.

The load-bearing insight: the sweep cells FAIL because the *correct code is not on
the tip branch*, not because the exit code lied. Making the reporting honest does
not add the missing code. That said, **#800 (the fail-stop itself) is still worth
fixing** — for honest reporting and to let independent pending leaves run — and it
is the *curative sibling* to the two grade-movers below (once each leaf's work
lands in its own lane, tolerating a legitimate no-op leaf as success gives the
plan coherent per-leaf diffs to accept). It is a correctness fix, not a
boundary-mover; sequence it under the two facets that actually move the boundary.

### 4b. Worker-grounding — leaf-scope the per-leaf worker (KEEP, risk: medium)

**The change.** Thread the already-existing-but-dropped leaf scope
(`Subtask.context`, `newt-core/src/plan.rs:207`, projected into `CrewTask.context`
by `to_crew_task`, `plan.rs:255/275`) to the worker and use it as a soft,
meet-only fence — the "feed it the actual target file path so it EDITS the
existing seam instead of inventing new files" lever. Today the scope is dropped:
`run_plan_with_reground` forwards only `goal`+`verify`
(`plan_exec.rs`, mirroring the existing conditional `verify` forward at
`plan_exec.rs:112-120`), and `parse_authored_plan` hardcodes
`context: Vec::new()` (`newt-cli/src/crew.rs:715`).

1. `plan_exec.rs`: when `task.context` is non-empty, add
   `args["scope"] = json!(task.context)`.
2. `newt-cli/src/crew_runner.rs` `LocalCrewRunner::dispatch` (crew arm): parse
   `args["scope"]`, pass a new `scope: &[String]` to `run_crew`/`run_team`.
3. `newt-scheduler/src/crew.rs` `run_crew` (`:243`): when scope is non-empty,
   (a) seed the navigator (`:280`) to prefer the scope files; (b) append a
   one-step directive to the planner prompt (`:365/:378`) — "You are implementing
   ONE STEP of a larger plan. Confine edits to `<scope>`. If the change is ALREADY
   present, emit NO edits — that is success, not failure."; (c) add a SECOND
   partition after `:416-418` routing out-of-scope edits into the existing
   `refused` feedback path (`:474-481`) instead of applying them. Empty scope ⇒ no
   clamp ⇒ byte-identical to today.
4. Populate the data: extend `PLAN_AUTHOR_SYSTEM` (`crew.rs:634`) to request
   `"files":["<real path>"]` per subtask and teach `parse_authored_plan`
   (`crew.rs:715`) to read them into `context`.

**Sharpen it (verdict improvement — do this, don't skip it).** Efficacy of step 4
is contingent on the same unreliable model self-reporting its scope; on the
sweep's one-shot-goal path `parse_authored_plan` hardcodes `context=Vec::new()`,
so steps 1–3 are inert plumbing until 4 lands, and the clamp is byte-identical to
today whenever the model under-declares. **Derive `Subtask.context` from the
harness's OWN grounding def-sites** — `definition_grep_pattern` /
`fetch_code_grep_hits` (`newt-cli/src/crew.rs:1039/1099`) already compute the real
definition file of the task's symbol — so the fence bites *deterministically* on
the sweep (e.g. the humanize-duration target file) and cannot be nulled by a model
that under-declares. Treat the model's `files` array as a fallback/augmentation,
not the source of truth.

**Expected lift on named cells.**
- *32b × T2 moves off FAIL* — the out-of-scope `Cargo.toml` edit is refused +
  fed back instead of landed, so the 3 retries focus on the real 1-line target.
  Honest caveat: not deterministic (the proposal claims a "clean shot," not a
  guaranteed flip); it needs 32b to target the right file (mitigated by the
  def-site derivation), the `Cargo.toml` edit to be the actual verify-failure
  cause, and 3 retries to suffice.
- *devstral24 × T2 stops passing BY ACCIDENT* — an "inspect" leaf can no longer
  illicitly write the whole fix; each leaf does its scoped part honestly. This is
  an integrity win (the sweep's stated goal — honest gates), not a cell flip, and
  it removes the mechanism that produces the downstream "leaf-2 nothing-to-land"
  and the `EXPERIMENT.md` vacuum files. Honest caveat: the accidental-fix case
  leans on the SOFT one-step *prompt* (an inspect leaf's declared scope plausibly
  includes the very target file), not the hard clamp; a capable model may ignore
  the prompt.
- *Composes with the nothing-to-land cells* (010, 30b) — this is the prerequisite
  that makes each leaf's work land in its own lane so #800's curative half has
  coherent per-leaf diffs to accept. Not claimed to flip them alone.
- *No regression on PASS cells* — r1:70b T2 is 1 leaf (scope = whole task, fence
  never bites); any undeclared-scope plan defaults to today.

**Risk: medium.** The real regression vector: a NON-EMPTY but WRONG model-declared
scope refuses a leaf's legitimate edit (`touched` empty, `refused` non-empty →
reprompt skipped → verify fails → `NeedsHumanReview`), converting a would-pass
leaf into a fail — which could silently break currently-passing multi-leaf cells
(devstral24 × 010). The def-site derivation is the primary mitigation (the harness
declares scope from ground truth, not the model). `run_team` gets `&[]` for
lead-decomposed subtasks (`SubtaskSpec` has no files field) → team-mode dispatch
stays unfenced until a follow-up; the fix covers the plan-exec crew path only.

**Honest / OCAP-safe / three-Cs.** The scope fence is a CONVENIENCE attenuation
*above* the real boundary, never the boundary (ADR 0005, engine-vs-boundary):
effective writable set = `worktree ∩ fs_write ∩ declared_scope` — meet-only, it
can only NARROW. The worktree + `is_safe_worktree_path` (`crew.rs:42`) + the
`fs_write` caveat stay the fail-closed boundary; model/human scope is UNTRUSTED to
widen (intersected in, never unioned). Empty scope grants no new authority
(default-off). It never touches the `verify` / locked-verify gate, so it cannot
make the behavioural grade easier to fake — it only RESTRICTS what a worker may
edit; "already-satisfied ⇒ no-op" is honest, and a badly-decomposed plan now fails
honestly instead of passing by over-reach. Three-Cs: scope is DATA
(`Subtask.context` / `files` in the plan TOML), composed at dispatch, human-
overridable (declare `context=[...]`), convention-driven (empty = unconstrained);
the one-step directive is a prompt string flagged to later move into the
`api_surface.rs` pure-data lexicon. Tests are the fully-mocked unit tier (MemWs +
RoleMock + the `plan_exec` MockRunner — no real fs/net/subprocess/clock).

### 4c. Planner-decomposition + router

#### Planner-decomposition — deterministically prune non-actionable leaves (KEEP, risk: low, #801)

**The change.** Add a pure, in-memory prune pass to `author_plan`
(`newt-cli/src/crew.rs:732-754`; both the first author and the re-author flow
through it), wired at `748-753`:
`parse_authored_plan(...).map(|mut p| { prune_non_actionable_subtasks(&mut p); p })`.
Encode the anti-pattern knowledge as pure DATA mirroring `routing.rs`'s
`READ_ROUTES`: `const ACTION_MARKERS: &[ActionMarker]` with
`enum MarkerKind { Inspect, Gate }`.
- **Inspect** verbs (inspect/examine/explore/investigate/understand/locate/
  identify/review/analyze) are read-only — the harness already reads the repo for
  the planner — so they are pruned wherever they appear.
- **Gate** verbs (verify/validate/test/build/run/check/confirm/ensure) are pruned
  ONLY when terminal (no other subtask names them in `deps`), since the harness
  auto-verifies EVERY subtask: a standalone terminal gate lands no diff, while a
  mid-plan gate a real leaf depends on is kept.

Classify by the LEADING token of `instruction` (so "Extract helper",
"Add validation", "Rename fn" survive — their leading verbs are not markers).
`fn prune_non_actionable_subtasks(&mut Plan)`: (1) empty-guard — if zero
actionable subtasks remain, RETURN untouched (never fabricate/half-prune a
degenerate plan; leave it for `plan_sanity`/re-author); (2) fixpoint loop removing
one prunable subtask at a time, splicing the removed subtask's own `deps` into
every survivor that named it (transitive dep-rewire) — **required** because
`next_ready_leaf` (`newt-core/src/plan.rs:128-135`) treats an absent dep as
never-`Done`, so a dangling reference would STALL the survivor. No
`PLAN_AUTHOR_SYSTEM` edit; the prune is the deterministic enforcement the prompt
(`crew.rs:634-646`) cannot guarantee.

**Sharpen it (verdict improvement).** Tighten the classifier so it can never
delete real work: consult the id-head only when the instruction's leading token is
absent or marker-vague (drop the bare id-head OR), and drop the polysemous Gate
verbs **build/run/check**, so "Implement signature verification" / id `verify-sig`
or "Build the parser" is never pruned. Dedup spliced `deps` (harmless to
`next_ready_leaf` but tidy).

**Expected lift on named cells.**
- *30b × T2 moves off FAIL* — the solid FAIL→PASS. 3 leaves → the inspect +
  terminal-gate leaves prune away, collapsing to the single diff-producing
  apply-fix leaf, so there is no intermediate nothing-to-land leaf to die on (30b
  passes T2 in single mode, so a 1-leaf crew run should PASS).
- *14b/32b × 010 — trap surface shrinks* (6 padded leaves → the read-only inspect
  + trailing validate/run-tests leaves prune; the >=3 real `extract` leaves
  survive, so the must-decompose contract holds). Honest caveat: the 010 lift is
  OVERSTATED as a flip — 010's seed already passes (gameable, no hidden spec) and
  the nothing-to-land there is plausibly worker over-implementation on the first
  real extract leaf (root cause #4, owned by 4b), which pruning does NOT remove.
  Observable change may be only turning a false-negative exit code into
  "complete."
- *devstral24 × T2/010 hardens* (drops the fragile accidental-fix inspect leaf and
  leaf-2 nothing-to-land) — stays PASS, more robustly.
- *No regression on r1:70b* (T2 = 1 clean leaf; 010 = 3 `extract` leaves — all
  non-markers, prune is a no-op).
- *Honestly out of scope:* 32b T2 (1 leaf, spurious `Cargo.toml`) and 14b T2
  (worker can't code) — single-leaf worker-side failures owned by 4b.

**Risk: low.** Contingency: lift is on UNOBSERVED leaf ids — the sweep gives leaf
counts and "intermediate nothing-to-land," not the actual ids; the
`[inspect, apply, gate]` shape is a reasonable but unproven assumption. Worst case
from an over-broad classifier is an *honest failure* (per-leaf verify + hidden
grade still fail), not a gamed gate — mitigated by the tightened classifier above.

**Honest / OCAP-safe / three-Cs.** A pure in-memory transform over the parsed
`Plan`, run BEFORE `grant_one_shot_authority`; it only deletes subtasks and edits
`deps`, never touches `caveat_policy` (default-deny preserved), never widens
authority, does no fs/net/exec. It removes only NO-diff leaves — never edits a
test, lowers a `verify`, nor marks anything passed; surviving code leaves still
face the full per-leaf verify + the hidden ungameable grade. It removes a
false-NEGATIVE (nothing-to-land) failure source, making grading MORE honest, and
cannot manufacture a pass. Three-Cs: the lexicon is pure DATA (`ACTION_MARKERS`),
read by a pure classifier, exactly the `routing.rs READ_ROUTES` model; a new
anti-pattern is a one-line data edit, with a droppable `[plan.prune]` override as
the follow-up seam. Tests are in-memory `Plan`/`Subtask` literals + the existing
mocked `PlanMock` dispatcher — parallel-safe, no real fs/net/subprocess/clock.

#### Single-vs-crew router — REJECTED (inert on the claimed cells)

Routing T2 to single mode instead of decompose would sidestep the crew failures —
but the classifier's own spec makes it INERT on exactly the cells `expected_lift`
claimed. Applying `grep_terms` (`crew.rs:957`) + `definition_grep_pattern`
(`crew.rs:1039`) to the real T2 prompt yields
`grep_terms = {humanize_duration, humanizes}`, whose definitions live in TWO files
— `src/util.rs:2` (`fn humanize_duration`) and `src/lib.rs:11` (the backticked
test fn `humanizes`). Condition-2 (<=1 def file) FAILS, so T2 classifies as
`GoalShape::Decompose` — the multi-leaf path that produces the failures. It
diverts NEITHER 30b/T2 NOR devstral24/T2. Plausible-but-inert; not doing it.

### 4d. Grading-integrity — REJECTED for this pass (part is inert on the sweep)

010 having no hidden `grade_spec` is a real gap (root cause #7). But the proposed
part-(b) fix — flipping `run.complete` — is INERT: `behavioral` is set by
`ratchet.sh` independently running `cargo test --test grade_spec`
(`scripts/eval/ratchet.sh`), while `plan_rc`/`run.complete`
(`newt-cli/src/crew.rs:626`) is only an emitted detail;
results/ratchet-findings.md shows T2 crew `plan_rc=0` WITH `behavioral=FAIL`,
proving they are independent. Changing `run.complete` moves no PASS/FAIL cell. The
*honest* remedy for 010 is to author a hidden `grade_spec.rs` for it (make the
rung ungameable, as T2 already is) — a seed-authoring task, tracked separately,
not a crew-code change. We do not do the grader change here.

## 5. Prioritized roadmap

Land order — biggest grade-lift and lowest risk first:

1. **Planner-decomposition prune (#801) — LAND FIRST.** Risk: low; the one solid
   FAIL→PASS (30b × T2); pure in-memory transform, fully-mocked tests, no OCAP
   surface. Ship with the tightened classifier (id-head only when
   instruction-vague; drop build/run/check from Gate verbs; dedup spliced deps).
   No dependency on any other facet.
2. **Worker-grounding leaf-scope (#800-adjacent) — LAND SECOND.** Risk: medium.
   Ship the def-site-derived scope (from `definition_grep_pattern`/
   `fetch_code_grep_hits`, `crew.rs:1039/1099`) as the source of truth, with the
   model's `files` array as fallback — this is what makes the fence bite
   deterministically and avoids the "byte-identical to today" inertness. Watch the
   regression vector (wrong scope refuses a legit edit) on the multi-leaf 010
   cells.
3. **Fail-stop curative half (#800).** After 1+2, make a legitimate no-op leaf
   report success rather than `Err`, so `run_plan_with_reground`
   (`plan_exec.rs:141-144`) tolerates the leaves that slip past the prune and the
   remaining independent pending leaves still dispatch. This is a correctness /
   honest-reporting fix, not a grade-mover (the ratchet reads the tree, not the
   exit code), so it lands *after* the two facets that move the boundary and
   depends on their per-leaf-diff coherence.

**Dependencies.** (1) and (2) are complementary and non-conflicting, but both edit
`run_plan_with_reground` — sequence them (prune first, then the `scope` forward).
(2) is the prerequisite that gives (3) coherent per-leaf diffs. (1) is the upstream
half of the fail-stop pair (stop *emitting* no-diff leaves); (3) is the downstream
half (tolerate any that slip through). The existing
`author_plan_decomposes_a_goal_via_the_model` test (`crew.rs:1576`, non-marker ids
`a`/`b`) must stay green — no regression to current tests.

**What to re-run on the ratchet to confirm the boundary moved.** After each land,
re-sweep the two tasks × the five models under crew mode and read the external
grade (not `plan_rc`):
- After (1): **30b × T2 flips FAIL → PASS** (the target confirmation); r1:70b and
  devstral24 stay PASS; 010 columns show fewer padded leaves (leaf-count drop) even
  where the grade doesn't flip.
- After (2): **32b × T2 moves off FAIL**; devstral24 × T2 no longer passes *by
  accident* (verify each leaf now edits only its lane — an integrity check, not a
  cell flip); confirm no regression on devstral24 × 010 (the wrong-scope vector).
- After (3): confirm the crew's *reported* completeness matches the external grade
  (no more false-negative exit codes over a tree that passes).

The success criterion for the whole effort: crew-mode T2 goes monotone across the
model sweep (no more devstral-PASS / 32b-FAIL / r1-PASS coin-flip), and single-mode
8/8 is matched by crew mode on the ungameable cells.

## 6. Out of scope / open questions

- **Team-mode dispatch stays unfenced.** `run_team` receives `&[]` scope because
  `SubtaskSpec` has no `files` field; leaf-scoping the lead-decomposed team path is
  a follow-up (add `SubtaskSpec.files`), not this pass.
- **010 needs its own hidden `grade_spec.rs`.** Until it has one, 010 remains
  `PASS?gameable` and any 010 "lift" is only a leaf-count / honesty change, not a
  trustworthy grade flip. Authoring the spec is a seed task, tracked separately
  from the crew-code facets.
- **The `[plan.prune]` droppable override** (three-Cs config seam for
  `ACTION_MARKERS`) is deferred — working code first (the `const` slice), config
  override next.
- **The one-step directive is still a prompt string**, flagged to migrate into the
  `api_surface.rs` pure-data lexicon rather than living inline in `run_crew`.
- **Open question — does the fence bite on abstract leaves?** For a leaf whose
  legitimate scope genuinely includes the target file (devstral's inspect case),
  only the SOFT prompt discourages the over-write; the hard clamp does not bite. A
  capable model may ignore the prompt. Measuring whether the prompt alone
  suppresses accidental over-reach — or whether a harder per-leaf edit budget is
  needed — is a Phase-2 question the re-sweep should answer.
- **Multi-trial confirmation.** Every sweep cell is n=1; the flips claimed above
  need >=3 trials per cell to separate signal from LLM non-determinism before any
  cross-model monotonicity claim is stated as a result rather than an observation.
