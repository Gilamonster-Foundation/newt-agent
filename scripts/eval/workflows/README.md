# Eval analysis workflows — canonical templates

The `*.workflow.js` files here are the **canonical, committed form** of the
crew-eval analysis suite (issue #805). They are Claude Code dynamic-workflow
scripts, but they live *here* — not in `.claude/` — so that a fresh machine
reconstitutes them from the repo alone:

```bash
scripts/eval/workflows/install.sh          # -> <repo>/.claude/workflows/ (generated, gitignored)
scripts/eval/workflows/install.sh --user   # -> ~/.claude/workflows/ (all projects)
```

After install they run as `/sweep-analyze`, `/ab-gate`, `/crew-autopsy`,
`/propose-verify`, `/grade-spec-author`. Claude Code is **one renderer** of
this methodology, not a dependency: everything load-bearing below is data +
method you can execute by hand, in CI, or from a future newt-native
orchestrator.

## Why this suite exists

The #802 sweep drew its headline from n=1 cells; #803's n=5 A/B showed the
headline cell was noise (~80% PASS) and the landed fix produced no lift.
Separately, the best moment of that investigation — a 15-agent adversarial
workflow rejecting five tempting-but-inert fixes before any code was written —
lived in one machine's session directory. Both lessons are institutionalized
here.

## The tool-neutral core

**Statistics are computed by code, never by a model.** Every number in a
verdict comes from the deterministic blocks embedded in each script (each
self-checks against known values at startup and refuses to run on mismatch):

- Wilson 95% score interval for per-cell pass rates (`/sweep-analyze`).
- Fisher's exact test (one-sided, hypergeometric) for A/B arms (`/ab-gate`).
  Calibration: cand 5/5 vs base 1/5 → p(one-sided) = 5/210 ≈ 0.0238.
- Min-n power advice: the smallest per-arm n at which the *observed* rates
  could reach significance — reported whenever a verdict is UNDERPOWERED.

**Honesty rules (data, not judgment):**

- `PASS?gameable` is never pooled with `PASS`; a rung without a hidden
  `grade_spec.rs` cannot certify a lift (verdict caps at UNGRADEABLE and
  points at `/grade-spec-author`).
- Cells with n < 5 are stamped UNDERPOWERED wherever they appear.
- Infra-excluded rows never enter denominators (sweep.sh already refuses to
  count them as trials — see `scripts/eval/RATCHET.md`).

**The 7-mechanism failure taxonomy** (from `docs/design/improving-crew-results.md`
§3, PR #802) with the evidence signature each classification must quote:
fail-stop; nothing-to-land; end-state-verify-on-intermediate-leaf;
worker-ignores-scope; worker-spurious-edits; planner-over-decomposition;
grading-integrity — plus `ops-noise` (excluded from denominators) and `other`.

**The inertness oracle** (what killed five plausible fixes in #802 §4a): the
ratchet grades the `crew/* | tail -1` tree against the hidden spec;
`plan_rc`/exit codes are diagnostics. A proposal that only changes reporting
moves zero cells. Every `/propose-verify` verifier is calibrated with the five
rejections (fail-soft exit codes, plan_rc flip, per-leaf compile-gate,
single-vs-crew router, run.complete flip) and must set `inert_on_grade`.

**Adversarial verification as the default shape:** find → try-to-refute →
only-confirmed-survives. Classifications below a confidence floor get a
skeptical re-read; grade specs must survive red-team gamers plus a
deterministic replay corpus; proposals must survive an inertness check.

## The improvement loop

```
sweep.sh (detached, n>=5)  ->  /sweep-analyze  ->  /crew-autopsy  ->  /propose-verify
                                                                            |
   /grade-spec-author (whenever a rung is PASS?gameable)                    v
                                                              implement lever (branch)
sweep.sh both arms  ->  /ab-gate: LIFT | NO-LIFT | UNDERPOWERED | UNGRADEABLE
```

Every hop is file-mediated (sweep dirs, autopsy JSON, design docs), so the
loop survives session boundaries and no workflow depends on another's
in-memory state.

## Security invariant

Inherited from `RATCHET.md`: model **names** only; no hosts, IPs, usernames,
or `$HOME` paths in these templates or in anything they write under
`results/` / `docs/`. Endpoints live in the operator's local `~/.newt`
config and are passed as runtime environment only.
