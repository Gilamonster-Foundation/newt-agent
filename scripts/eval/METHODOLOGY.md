# Methodology — the autonomous #548 self-hosting evaluator

How we measure whether `newt`'s autonomous `--one-shot` loop can implement a real
issue (#548), and how we isolate the effect of features landed on `main`. This is
the durable "how", separate from any one run's numbers (those live in
`EXPERIMENT.md` + `results/{A,B,C}-*.md`).

## 1. What we are measuring

The **North-Star evaluator** for newt-agent: can the agent, given only the issue
URL, autonomously produce a *working* implementation? Issue #548 ("roll up the
verbose `/dgx` help into one top-level line; keep `/dgx help` as the
progressive-disclosure detail page") is the fixed task — small, real, and
behaviorally checkable.

"Pass" is **not** "the loop finished" or "`just check` is green". Pass is **the
feature actually works when you run the binary.** Keeping those distinct is the
whole point.

## 2. The instrument: a behavioral grader

`scripts/eval/grade-548.sh` drives a *built* `newt` in lean/pipe mode
(`--plain --ephemeral`, fed a slash command then `/exit`) and inspects the real
help output:

| signal | meaning | pass condition |
|---|---|---|
| `top_dgx_subs` | `/dgx <sub>` lines at the **top-level** `/help` | rolled up ⇒ **≤ 1** |
| `dgx_help_subs` | `/dgx <sub>` lines under **`/dgx help`** | disclosure kept ⇒ **≥ 5** |
| `pass` | both above | true |

Output: one JSON line on stdout (collected into the data sets) + a human report on
stderr; exit 0 on PASS.

**Why behavioral, not `just check`.** The crew's per-leaf gate is `just check`
(build + unit tests) — necessary but **not sufficient**. Run A produced a
`dgx_help.rs` module that compiled and "passed" but was an **orphan** (never
`mod`-declared, never wired into `help_lines`): green gate, no feature. A pure
build/test gate cannot tell "implemented" from "plausible-but-inert". Driving the
actual binary can. (This held across all three runs — see §6.)

**Calibration.** On an unmodified baseline the grader must FAIL (the feature
isn't there) — confirmed: `top_dgx_subs=8`. It also correctly reports
`disclosure=true` because `/dgx help` *already* expands in the baseline, so the
grader measures exactly #548's *remaining* work (the top-level rollup), not the
part already done. A correct implementation ⇒ `top_dgx_subs:0, pass:true`.

## 3. The experiment: fixed instrument, one variable

The grader is the **fixed measuring instrument**. The **only** independent
variable is the codebase the autonomous loop runs against. Each run:

1. **Rebase** the `eval/548-grader` branch (grader + harness) onto a chosen commit
   — this records exactly which features are present.
2. **Throwaway checkout** pinned to that same commit (the loop must not mutate the
   real tree, and each run starts from an identical clean state).
3. **Run the identical eval:**
   `newt plan --goal "<#548 url> … come up with a plan to implement it"
   --one-shot --dir <throwaway> --max-leaves 12`.
4. **Grade** the consolidated result; record JSON + the diff + wall-clock.

### Held constant across runs
Prompt; `--max-leaves 12`; a warm shared `CARGO_TARGET_DIR` (so per-leaf build
time is comparable, not cold-vs-warm). In Exp. 1 the whole crew is held constant;
in Exp. 2 the planner (`nemotron-3-nano:30b`) + triage are held constant and only
the navigator changes. Home-network specifics (hostnames, GPUs) are intentionally
**not** in any committed file — they live in the operator's local config only;
run records name *models* (`qwen3.6:27b`, `gpt-4.1`) as identities, not hosts.

### The runs (cells)
Two sub-experiments, one fixed grader. C is the shared cell (it's the last
codebase cell *and* the baseline executor cell).

**Exp. 1 — codebase as variable** (crew constant):
| run | commit | feature increment under test |
|---|---|---|
| **A** | `68c9b2c` | baseline |
| **B** | `41cb1de` | + #661 compaction/summarizer series (#666/#667/#668) |
| **C** | `d25662d` | + #669 workspace-API knowledge base |

**Exp. 2 — executor (navigator) model as variable** (codebase `d25662d` constant):
| run | navigator model | kind |
|---|---|---|
| **C** | `qwen2.5-coder:14b` | local, coder (shared baseline) |
| **D** | `qwen3.6:27b` | local, general |
| **E** | `gpt-4.1` | frontier, external |

Pairwise diffs isolate one variable each: **A↔B**, **B↔C** (one feature
increment); **C↔D**, **C↔E** (one executor swap).

## 4. Two readouts per codebase

1. **Deterministic regression check** — grade the codebase's *own* binary (no LLM,
   no eval). Noise-free: does the feature itself change the #548 surface / regress
   help? (All three: `top_dgx_subs=8`, no regression.)
2. **Stochastic autonomous eval** — the full loop (§3). This is LLM-driven, so it
   is the noisy readout (see §5).

## 5. Stochasticity & validity (read before drawing conclusions)

The planner and crew are LLM-driven and non-deterministic. With **one trial per
cell**, *process* differences between runs (leaf counts; which files got touched;
orphan-module vs README-gut vs no-op) are **within run-to-run noise** and must
**not** be attributed to the feature increment. What *is* robust:

- the **deterministic** check (§4.1) — zero noise;
- the **grader outcome** (`top_dgx_subs`, `pass`) — a coarse, stable signal: when
  it is *identical* across all cells (as here), "no feature moved it" is a safe
  read even at n=1, because a real implementation would have produced an
  unmistakable swing (8 → 0).

Statistical attribution of a feature's *effect size* (e.g. "does richer context
raise the implementation rate from 0% to X%?") requires **multiple trials per
cell** and a success-rate metric. That is the natural next iteration of this
harness, not something one trial each can support.

## 6. Threats to validity (and how this design handles them)

- **"Green gate ≠ working feature."** Addressed by the behavioral grader (§2).
  This is the headline methodological contribution.
- **Run mutates the real repo.** Addressed by the throwaway checkout (§3.2).
- **Cold vs warm build skews cost.** Addressed by the shared warm
  `CARGO_TARGET_DIR`.
- **Feature secretly changes the help surface.** Addressed by the deterministic
  check (§4.1) before trusting the eval.
- **Confounded increments.** Addressed by rebasing one feature-set at a time so
  each pairwise diff is a single increment.
- **n=1 over-claiming.** Addressed by §5 — we only claim what the coarse, stable
  signals support.

## 7. Reproduce

```bash
# behavioral grade of any built newt
./scripts/eval/grade-548.sh <path-to-newt-binary>

# one autonomous run against a throwaway checkout pinned to <sha>
git clone . <throwaway> && git -C <throwaway> checkout <sha>
CARGO_TARGET_DIR=<warm-target> \
  newt plan --goal "<#548 url> … implement it" --one-shot --dir <throwaway> --max-leaves 12

# regenerate the charts from the recorded run data
source ~/venv/bin/activate && python scripts/eval/charts.py
```

Artifacts: `EXPERIMENT.md` (numbers + charts + learnings), `results/{A,B,C}-*.md`
(raw per-run records), `results/chart-*.png`, `charts.py` (chart source),
`grade-548.sh` (the instrument).
