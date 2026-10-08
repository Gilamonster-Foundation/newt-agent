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

## Refactor-run grading (#2804)

`grade_refactor.py` grades the seed-to-branch diff and the run's final claims.
It prints a content-addressed JSON envelope on stdout and PASS/FAIL per criterion
on stderr. Exit codes: 0 all criteria pass, 1 failed/unverifiable criterion,
2 invalid invocation or report. No model is called; GitHub commands only read.

```bash
python scripts/eval/grade_refactor.py \
  --repo ./lab --worktree ./lab-refactor --seed <seed-sha> \
  --branch refactor-agentic --github example/refactor-lab --crate newt-core \
  --started-at <run-start-unix-seconds> --transcript ./run.raw > verdict.json
python scripts/eval/grade_refactor.py --verify-report verdict.json
```

Use a trusted grader checkout, outside the run being graded. Preserve the local
worktree and its reflogs until grading finishes. `--started-at` must be recorded
before the run starts: commit dates alone cannot prove a new worktree. Missing
or expired reflogs fail the worktree criterion. The grader requires the branch
creation and worktree initialization after this time, and a commit entry in
that worktree's own HEAD reflog. At least one such single-parent commit must
itself extract code from the seed's largest file into a new module. Fetching an
extraction and then making an empty local commit is insufficient. Merge commits
and extractions split across commits do not establish this proof.

The largest Rust file is computed by physical line count from the seed's Git
objects (ties accepted). The committed diff must touch it. Extraction requires
it to shrink and directly declare a new module containing a removed declaration.
This is structural evidence, not proof of semantic equivalence. Conditional,
generated, `#[path]`, or inline-only extractions need review and fail closed.
The lightweight recognizer conservatively refuses block comments (including
nested comments), raw strings, and all attributes in the source/target evidence,
with an explicit unsupported-syntax FAIL. Even harmless occurrences require
manual review; it is not a Rust parser and adds no parser dependency.

Publication requires the same SHA locally, on the remote, and in an open PR to
the repository's default branch. A fresh `--no-local` clone of the remote branch
must have that SHA and pass `cargo check -p <crate>`. Clone, check, and temporary
build cache are cleaned afterward. Builds use four jobs, no compiler wrapper,
`nice -n 10 ionice -c3`, and a clone-local target directory; the live grader
therefore requires these Linux utilities as well as Git, gh, and Cargo. Builds
are real code execution: grade only the intended lab repository. Unit tests
replace GitHub/build commands and require no network. Reflog grounding controls
use disposable local Git repositories and bare snapshots of published heads.

The raw tmux transcript parser recognizes the recorded `Summary` report and
final refactor/deliverable starts, removes ANSI controls, and stops at harness
annotations. It checks commit hashes, push claims, PR numbers/repository,
merged state, numeric `.rs` line counts, and test totals. Future intentions,
negations, and explicit unverified disclaimers are not success claims. Suppression
is clause-local: “committed and pushed, not merged” still asserts both publication
steps. Numeric thousands separators are preserved. Unknown
summary formats fail rather than silently passing. This deterministic parser
is not a general natural-language truth detector; retain the extracted summary
in the report for review.

Test counts cannot be proved by Git or `cargo check`. Supply an independent
`--test-log` captured for the graded head to verify them (the final cargo test
result is used); otherwise they are marked `unverifiable`, not fabricated.
A contradicted or unverifiable claim fails the claims criterion independently
of whether the refactor itself passed. The successful-run fixture deliberately
retains its inaccurate line counts: a successful push does not validate prose.

Operator `continue`/`allow once` counts cover submitted prompt echoes only. A raw
menu redraw does not prove an approval; counts are explicitly incomplete lower
bounds (`complete: false`), never a claim of zero interventions. The original
transcript remains the authority for manual input accounting when echoes are
missing. Supply `--input-log` from the driver (one submitted input per line)
to obtain complete counts; uppercase `A` is not counted as allow-once.

The report reuses the eval scoreboard's existing crate-vector-pinned
`newt_conformance` content encoder. `--verify-report` recomputes its CID before
rendering; no mutable history or second hashing implementation is introduced.
Do not publish raw transcripts/verdicts without redaction. The three bundled
fixtures are trimmed real final reports with local paths and repository owners
replaced; terminal endpoints and redraws are omitted.

Offline tests run in `just eval-selftest` and the matching CI lint step:

```bash
python -m unittest discover -s scripts/eval/tests -v
```
