# Next loop levers — driving local-model tasks to done

**Status:** decision menu (draft PR for discussion — nothing here is
implemented by this PR) · **Companion:**
[`next-crew-lever.md`](next-crew-lever.md) (crew-side; this doc is the
single-agent loop side) · **Evidence:** live-session forensics
2026-07-06 (ornith:35b on dgx1, newt 0.7.1) + source citations at
`305d56d`. Every mechanism claim below was adversarially re-verified
against source or the conversations DB before inclusion; two findings
from that review materially reshaped the menu (see §2.6 and L5).

## 1. The incident

Task: *"Come up with a plan to fix issue #969"* on a 35B local model.
What the operator saw: the model works for minutes, narrates its next
step ("Let me look at exactly what OCAP's reason field looks like…"),
and stops. Typing `continue` buys another few minutes, then it stops
again. Two turns of forensics (conv `…456f38c2`, seqs 304–305):

- **seq 304** — 25 rounds, 30 tool events, 19,416 tokens in. The model
  never created a plan ledger (`update_plan` uncalled, scratchpad `{}`)
  — and, notably for a *plan request*, went off-script and made **three
  successful `edit_file` calls plus a git op**. At the cap its
  tools-disabled summary described future actions, was discarded, and
  with no plan/scratchpad to salvage the assistant output was **only**
  the 336-char cap banner. 25 rounds, real edits made, zero
  user-visible findings.
- **seq 305** (`continue`) — re-fetched the same issue page seq 304 had
  already fetched (`args_digest b3:63499f22112cea56` in both), did call
  `update_plan` (twice — a 4-step ledger now exists), and ended on a
  dangling "Let me look at…" narration after its one narration nudge
  was spent.

The stall is not only a cap phenomenon: seq 225 (07-04, same week)
ended on a dangling "Let me read…" after just **3** tool calls — the
single narration nudge had already been spent, so the next narration
was accepted as the final answer. And the waste compounds across
sessions: in the two issue-548 sessions examined, the same issue page
was fetched 9 times across 7 turns (the week-wide count is higher);
turn 302 (07-06) reported "Plan status: 0/5 done" and "Created branch"
for the same task turn 223 (07-04) had already driven to 3 commits.
Discovery is redone because nothing durable survives the turn boundary.

## 2. Root-cause chain (each link verified)

1. **The cap is the operator's own pin.** `~/.newt/config.toml` sets
   `[tui] max_tool_rounds = 25` — the *old* default. The shipped default
   is now 40 (`newt-core/src/config.rs:1601-1603`, doc comment
   `config.rs:1433-1444`: "raised from 25"), plus
   `workflow_grace_rounds` (default 5, `config.rs:1605-1607`).
2. **Compression fires by message count, not context pressure.** The
   trigger is `messages.len() > threshold` (`compress.rs:341`); the TUI
   clamps the configured threshold (40) to `max_tool_rounds − 3 = 22`
   (`newt-tui/src/lib.rs:9190-9202`). Token headroom is irrelevant: at
   the observed firing (~9.7k est. tokens) ornith's ~209k budget could
   never fire the token guard (`compress.rs:358-366`). So any
   tool-heavy turn gets its working set halved mid-flight, and the
   trigger can re-fire as the list regrows.
3. **Compression steers the model back out to re-read.** The compaction
   message carries a lossless escape hatch — a whole-span
   `memory_fetch("compaction:<id>")` handle (`compress.rs:662-674`) —
   but two lines away its reread breadcrumb says the summarized files'
   contents are not preserved: "RE-READ any you rely on"
   (`compress.rs:1087-1133`). The observed behavior is that the model
   follows the breadcrumb, not the handle: re-reads burn the remaining
   rounds, and the read-only-exploration nudge (`mod.rs:1302-1322`)
   piles on more messages, regrowing the list toward the next trigger.
4. **The summarizer is misconfigured into its own documented failure
   mode.** `~/.newt/summarizer.toml` is absent (only `.backup` files
   remain); an absent file silently means "reuse the session backend"
   (`config.rs:2890-2901`, `newt-tui/src/lib.rs:8831-8866`) — so
   mid-loop compaction runs **ornith:35b on the contended dgx1** with a
   60s timeout, 1 retry, and **no fallback model**. That violates every
   rule the backup file encodes (run on gnuc, never a thinking model,
   150s timeout, explicit fallback); since ornith `emits_thinking`, a
   thinking-only reply reads as an empty summary → static marker
   (`lib.rs:4099-4107`, `compress.rs:1047-1049`). This is not
   theoretical: on 07-05 four consecutive capped turns (conv
   `…4cc3430a`) carry "*and the final summarization request also
   failed*" — the summarizer was failing even at cap-exit.
5. **Hallucination recovery burns rounds.** Rounds advance regardless
   of what happened in them (`'round_loop: for round in 0..hard_tool_rounds`,
   `mod.rs:1240`; no refund path). Turn seq 301 (conv `…c21ecf03`) had
   4 hallucinations — all `run_command` calls whose "command" was a
   *tool name* (detected by `is_hallucination`, `tools.rs:758-772`;
   recorded in the `phantom_reaches` turn column) — costing ≥4 and
   realistically ~8 of 25 rounds once corrected retries are counted.
6. **A plan-less turn can't earn the grace window.** The +5 grace
   rounds at the soft cap require error-evidence, or an **active plan
   step** plus recent progress (`mod.rs:2723-2762`; progress =
   `update_plan`/`write_file`/`edit_file`, `mod.rs:2983-2989`).
   Seq 304 *had* qualifying progress — three successful edits — but no
   plan ledger, so no active step, so no grace. The gap is **plan-less
   turns**, not read-only ones: without a plan, progress counts for
   nothing at the cap, the pending-plan gate never fires, and cap-exit
   has nothing to salvage.
7. **At cap exit, an "I'll do X next" summary is discarded.** The
   tools-disabled final summary is classified; `PendingAction` ⇒ the
   model's text is replaced wholesale by the canned fallback
   (`cap_exit_summary_is_action_handoff`, `mod.rs:3441-3447`;
   fallback `mod.rs:3424-3439`). Salvage is only the `<plan>` +
   `<state>` ledgers (`mod.rs:2933-2941`) — both empty in seq 304,
   hence a turn that ends with *nothing*.
8. **There is no cross-turn auto-continue.** Every turn ends back at
   the human prompt; the in-turn nudge ladder (narration cap 1,
   pending-plan cap 1, stale-file cap 1, thinking-only cap 2 —
   `mod.rs:3020-3032`) is the only "continue" machinery, and the
   narration nudge's cap is hard-coded (the code comment itself
   nominates a `[tui] narration_nudge_cap` knob).

Not a cause: `inference_timeout_secs = 120` is per-HTTP-request (whole
request on probes, idle-per-chunk on streams — `mod.rs:1084-1100`), and
no request tonight approached it. One outlier turn (777s, conv
`…c21ecf03` seq 301) was 59% a human permission prompt (462s on one
`request_permissions`).

## 3. What already shipped (and why it wasn't enough)

Eleven PRs landed 07-04 → 07-06 as a layered defense: repeat-call guard
(#928), plan-bootstrap nudge (#929), narration-intent verbs (#933),
stale-file ground-truth (#934), pending-plan completion gate (#935),
workspace-state block (#936), thinking-only retry escalation (#939),
Ollama-XML retries (#927) + root-tag recovery (#938), `/rounds` session
override (#931), and round-cap grace + output tails + `memory_fetch`
grep (#942). Plan **re-seat** — the validated anti-drift fix (12/12 vs
8/12 on 12-step plans,
[weak-model-plan-mode-findings](../research/weak-model-plan-mode-findings.md))
— also ships **default-on** (#631, `mod.rs:1282-1290`). Roadmap
Phase 27 (loop hardening for weak local models) is the open umbrella.

Tonight's session ran a 0.7.1 build **with all of that active** and
still stalled, because nearly the whole defense keys on the plan
ledger — and the model never created one. No plan ⇒ no pending-plan
gate, no grace eligibility (despite real edit progress), no cap-exit
salvage, nothing for re-seat to re-seat. The plan-bootstrap nudge
(#929) only fires on a second *empty `plan_get`* — a model that never
polls its plan is never steered. Meanwhile the one narration nudge per
turn is quickly spent, and compression + the reread breadcrumb convert
the remaining rounds into re-exploration. The gap is **getting a plan
to exist early**, plus **not losing state at compaction** — not more
nudges of the shipped shape.

## 4. Decision menu

### Tier 0 — operator config, no code (deployable tonight)

**T0.1 — unpin the round cap; tune per model.**
`[tui] max_tool_rounds = 40` (or delete the pin to take the shipped
default), keep `workflow_grace_rounds = 5`; optionally
`[[model_tuning]] model = "ornith:35b"` overrides, and `/rounds double`
live when a task is mid-flight.
*Buys:* room to finish; also **moves the trim clamp from 22 → 37
messages**, delaying the first compression well past where most turns
end today. (A `mid_loop_trim_tokens` budget can be *added* for giant
single messages, but note honestly: it cannot replace or defer the
message-count trigger — the clamp is the only lever over that.)
*Failure mode:* the roadmap's own warning — "Raising `max_tool_rounds`
only lets it thrash longer" (ROADMAP.md:1326). Pair with T0.2.

**T0.2 — restore `summarizer.toml`.** Copy the `.backup` back into
place (summarizer on gnuc `gemma3:12b`, 150s timeout, fallback
`qwen2.5-coder:7b`, `keep_alive 30m`).
*Buys:* compaction stops contending with the session model, stops
risking thinking-only empty summaries → static markers (observed ×4 on
07-05), and stops adding up to ~120s per fired trim.
*Failure mode:* none known — this restores the configuration the
backup was explicitly written to preserve. Cheapest,
highest-confidence fix.

**T0.3 — probe the model's tool-call conformance.** Run `/probe`
against ornith:35b — its conformance entry is unprobed
(`tested_date: ""`; the window tuning, by contrast, is already
high-confidence as of 07-04). Given seq 301's four
tool-name-as-command hallucinations, a measured conformance profile is
the missing datum.
*Buys:* measurement where there is currently a default.
*Failure mode:* none; a few minutes of dgx1 time.

**T0.4 — route open-ended diagnostics to crew/team.** The config's own
doc comment (`config.rs:1438-1442`) advises exactly this: open-ended
work should reach for `newt crew "<task>"` (bounded `--max-attempts`,
honest `NeedsHumanReview` exit) rather than an unbounded chat cap.
The `~/.newt/crews/home.toml` drop-in already routes planner=dgx1,
navigator/triage=gnuc.
*Failure mode:* crew quality on this hardware is its own project
(see [`improving-crew-results.md`](improving-crew-results.md)).

### Tier 1 — small PRs (one knob or gate each)

**L1 — plan-by-round-N gate for local loadouts.** *(The keystone: the
incident's evidence-matched fix.)* If a turn passes round N (say 5)
with multiple distinct tools used and no plan ledger, inject one
plan-bootstrap nudge ("call `update_plan` now with a short ordered
plan"). Placement matters: fold it into the read-only-exploration
nudge's no-plan branch or inject at round start, so it doesn't collide
with the existing no-tool-call ladder (`mod.rs:1736-1858`) — and note
every nudge is a message that feeds the count trigger (§2.2), so cap
it at 1.
*Buys:* makes the entire plan-keyed defense stack — pending-plan gate,
grace, cap-exit salvage, re-seat — reachable for models that don't
spontaneously plan. Seq 304 failed *every one* of those for want of a
plan ledger.
*Failure mode:* one more message per turn on plan-less models.

**L2 — a no-plan grace arm.** Add a third eligibility branch to
`cap_grace_nudge` (`mod.rs:2723-2762`): grant the (already bounded)
grace window when the turn shows **novel evidence** in the last K
rounds — first read of a file, first fetch of a URL — *without*
requiring an active plan step. Substrate: extend
`claim_check`'s per-round observed-paths collection (`mod.rs:1231`)
rather than `RepeatCallGuard` (which only memoizes shell probes and
failures). Stated plainly: under today's rules seq 304 had qualifying
progress and still got no grace because it had no plan; L2 as a
*progress-definition* tweak would not have helped — it must be a new
eligibility arm, or it only pays off after L1 lands.
*Failure mode:* an aimless reader gets +grace rounds of aimless
reading; bounded by `workflow_grace_rounds`.

**L3 — `[tui] narration_nudge_cap` knob.** Promote the hard-coded
`NARRATION_NUDGE_CAP = 1` (`mod.rs:3020` — the comment already
nominates the knob). Default 1; weak-local-model operators set 2–3.
*Buys:* each increment converts one more "Let me look at…" stall into
an automated in-turn continue — seq 225 stalled this way after only 3
tool calls, and seq 305 after its single nudge was spent.
*Failure mode:* a model in a genuine narration loop burns cap+N
rounds; keep it small and per-model-tunable.

**L4 — finish honest cap-exit (Phase 27.5).** When rounds were
dominated by hallucination corrections / repeats / dead rounds, the
banner should say that instead of "raise `[tui].max_tool_rounds`"
(tonight that was anti-advice); and when plan+scratchpad are empty,
salvage what *does* exist deterministically: claim-check-verified
paths, files read and edited, the repeat-guard evidence list.
*Buys:* a capped turn always hands the human (and the next turn) a
non-empty, honest ledger — turns like seq 304, which had three real
edits to report, stop being total losses.
*Failure mode:* banner grows; keep it structured.

**L5 — point the compaction breadcrumb at the handle.** Reword
`reread_breadcrumb` (`compress.rs:1087-1133`) so its first instruction
is "fetch the verbatim span via `memory_fetch("compaction:<id>")`,
then re-read only what the fetch doesn't cover" — today the breadcrumb
says "RE-READ any you rely on" while the handle sits two lines away
unused (§2.3).
*Buys:* plausibly the cheapest attack on the re-exploration spiral —
one prompt-string PR, no new mechanism.
*Failure mode:* models that ignore instructions ignore this one too;
measure fetch-vs-reread rates before and after.

### Tier 2 — structural (the "make better plans" tier)

**F1 — `plan_mode` + `effort` techniques**
(designed in
[`thinking-effort-and-plan-mode.md`](thinking-effort-and-plan-mode.md),
pre-build). Two-phase driver: high-effort **read-only** planning turn
that emits a canonical `Plan` persisted to `plan.md` (durable —
survives compression where free-form reasoning does not), optional
human approval gate, then low-effort execution against the plan.
*Buys:* the direct answer to "get the model to make better plans" —
plans become an artifact, not vibes; the artifact is exactly what the
pending-plan gate, grace window, cap-exit salvage, re-seat, and
`resume_context` all key on. It would also have prevented seq 304's
off-script edits: a plan request should not be editing files, and
plan-phase read-only caveats enforce that.
*Presupposes:* the `<think>` split (#385).
*Measured by:* plan-quality + completion-rate sweep, n≥5.

**F2 — progressive-disclosure compaction (Step 20.4).**
Today's compaction already stores the evicted span and appends **one
whole-span** `memory_fetch("compaction:<id>")` handle
(`compress.rs:662-674`, wiring `lib.rs:6400-6404`, `6546-6547`). The
gap vs
[`progressive-disclosure-compaction.md`](progressive-disclosure-compaction.md)
is granularity and defaults: **per-item page handles + one-line gists
replacing the lossy summary** (`compaction_mode = disclosure`), the
anti-thrash latch, and reconciling the contradictory in-message
guidance (L5 is the cheap forerunner).
*Buys:* kills the re-exploration spiral at the source ("never lose"):
a compacted read costs one `memory_fetch`, not a re-read round.
*Failure mode:* re-page thrash; the design doc specifies the latch.

**F3 — bounded cross-turn auto-continue.** Verified gap: nothing in
the interactive path ever re-submits a turn. Add
`[tui] auto_continue_turns = 0` (default off): when a turn ends
(a) at the cap with an unfinished plan ledger, **or** (b) with a final
answer the existing classifier marks `PendingAction`
(`cap_exit_summary_is_action_handoff`, `mod.rs:3441-3447` — without
(b) the trigger would have fired zero times tonight, since seq 305
ended by narration, not at the cap), newt re-submits "continue" up to
N times with an honest banner counting attempts, then stops.
*Hard rules:* never auto-continue while a permission request is
pending; require new progress between auto-continues (reuse L2's
novel-evidence test); N small. *Depends on:* L1 in practice — with no
plan ledger, trigger (a) never fires.
*Buys:* the literal "keep the system going" ask — tonight's manual
`continue` loop, automated and bounded.
*Failure mode:* the sharpest tool here — it multiplies whatever the
loop does, including thrash. Off by default.

**F4 — model-family profiles**
([`model-family-profiles.md`](model-family-profiles.md)). Land the
profile bundle so "model implies the profile" and ornith gets tuned
knobs (rounds, grace, trim, nudge caps) without hand-editing config —
values discovered by sweep, not guessed.
*Buys:* every lever above becomes per-family data instead of global
config — the three-Cs ending for this whole menu.

## 5. Recommended sequencing (for discussion)

1. **Tonight (T0.2, T0.1, T0.3)** — restore the summarizer, unpin the
   cap, probe conformance. Zero code; removes the contention/empty-
   summary waste and the tightest artificial limits. Rerun the exact
   #969 planning task as the yardstick.
2. **This week (L1 → L5 → L3 → L2 → L4)** — plan-gate first (it makes
   the whole shipped stack reachable and is the incident's
   evidence-matched fix), then the one-string breadcrumb fix, then the
   nudge knob; L2 and L4 round out grace and honest exits. Each is
   one-file-plus-tests-sized.
3. **Next (F1, then F2)** — `plan_mode` is the better-plans
   investment; disclosure compaction is the never-lose investment. F3
   (auto-continue) only after L1+L2 exist to gate it; F4 wraps the
   results into profiles.

Per house discipline: each tier-1+ lever gets a sweep verdict (n≥5,
`/ab-gate`) before it's declared a grade-mover — #803 taught us
plausible ≠ measured.

## 6. Out of scope

- Crew/team execution quality (owned by
  [`improving-crew-results.md`](improving-crew-results.md) /
  [`next-crew-lever.md`](next-crew-lever.md)).
- Summarizer *quality* work beyond configuration (Phase 24 owns it).
- OCAP/permission UX (the 462s permission-prompt observation is real
  but orthogonal — worth its own note).
- Any implementation: this PR is the menu, not the meal.
