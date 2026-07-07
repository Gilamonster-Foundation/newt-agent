# Autonomous #548 evaluator — behavioral grader + A/B harness

A controlled experiment to measure the **autonomous `--one-shot` loop's ability
to actually implement issue #548** (roll up the verbose `/dgx` help into a single
top-level line + keep `/dgx help` as the progressive-disclosure detail page), and
to isolate the effect of features landed on `main`.

## Why a *behavioral* grader

The crew's per-leaf gate is `just check` (build + unit tests). That's necessary
but **not sufficient**: the first eval run produced a plausible `dgx_help.rs`
module that compiled and "passed" — but it was an **orphan** (never `mod`-declared,
never hooked into `help_lines`), so `/dgx` help was unchanged. `just check` is
green; the feature does not exist.

`grade-548.sh` closes that gap. It drives a built `newt` (lean/pipe mode) and
inspects the **actual help output**:

- `top_dgx_subs` — # of `/dgx <sub>` detail lines at the top-level `/help`
  (rolled up ⇒ **≤ 1**).
- `dgx_help_subs` — # under `/dgx help` (progressive disclosure ⇒ **≥ 5**).
- **PASS** ⇔ rolled up **and** disclosure kept.

```
./scripts/eval/grade-548.sh <path-to-newt-binary>   # JSON on stdout, report on stderr
```

> Note: `newt` connects to the session backend on startup, so a reachable backend
> is needed for it to print help (the help text itself is backend-independent).

## The A/B experiment

The grader is the **fixed instrument**; the codebase under it is the **only
variable**.

1. **Data set A — baseline.** This branch (`eval/548-grader`) is cut from
   `68c9b2c`, *before* the new features being landed separately. Run the #548
   `--one-shot` eval against a throwaway checkout, build `newt` from the result,
   grade it.
2. **Data set B — with the new features.** Rebase this branch onto current `main`,
   re-run the identical eval + grade.
3. **A vs B** isolates whether the new features move the autonomous loop closer to
   a real #548 (e.g. does `top_dgx_subs` drop? does `pass` flip?).

Results live in `scripts/eval/results/`.

## Baseline grader sanity (`68c9b2c`)
The unmodified baseline binary FAILS — as it must (#548 not yet implemented):
```json
{"issue":548,"top_dgx_subs":8,"dgx_help_subs":8,"rolled_up":false,"disclosure":true,"pass":false}
```
A correct rollup ⇒ `top_dgx_subs:0, rolled_up:true, pass:true`.

## Sibling: the loop-completion track (`grade-loop.sh` / `loop-sweep.sh`)

Where `grade-548.sh` asks "does the *feature* work?", `grade-loop.sh` asks "did
the *turn* reach a usable done state, or die the incident's death?" — the
yardstick for the [next-loop-levers](../../docs/design/next-loop-levers.md)
work. It drives `newt` through the verbatim
[yardstick prompts](../../docs/design/evidence/next-loop-levers-yardstick.md) in
an **isolated `$HOME`** and reads plan-ledger / cap-salvage / dangling-narration
/ phantom-reach signals out of the run's own `conversations.db`.

```
./scripts/eval/grade-loop.sh <newt-binary> --home <throwaway-with-.newt>  # one trial
./scripts/eval/loop-sweep.sh --out results/loop-sweeps/<name> --newt <bin> \
    --levers baseline,T0.1,T0.2 --trials 5 --scratch /var/tmp/loop-sweep   # n≥5 A/B
./scripts/eval/grade-loop.sh --self-test   # offline; no backend
```

Full method, dgx1 substrate, and the per-lever cells:
[`docs/design/evidence/next-loop-levers-testplan.md`](../../docs/design/evidence/next-loop-levers-testplan.md).
The endpoint template (host stays local) is `loop-template.example`.
