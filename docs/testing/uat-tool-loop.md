# Live behavioral tool-loop UAT

A **live** User-Acceptance suite that drives the real `newt` harness with
user-phrased coding prompts on disposable workspaces against a real LLM, and
asserts not just *task correctness* but the **tool-loop behaviors** that the
Phase-27 loop hardening targets (tool-alias/corrective feedback, dup-guard,
default-on plan ledger, honest cap-exit, honest behavior under an unmet
precondition).

**Runner:** [`scripts/uat_tool_loop.sh`](./scripts/uat_tool_loop.sh).
**Results land in:** `docs/testing/results/` (one dated doc per run).

## Where it sits (relative to [`bat-uat.md`](./bat-uat.md))

`bat-uat.md` defines the BAT/UAT tiers and two existing mechanisms:

| Mechanism | What it asserts | Where |
|---|---|---|
| `newt-eval` golden-diff cases | **diff correctness** on fixed cases (`diff_applies`/`rust_compiles`/`tests_pass`) | mock per-PR (`mock_e2e`), live weekly (`eval-live.yml`) |
| `tool_round_cap_tests` (mocked) | **loop hardening** vs a *scripted* model (e.g. `uat_thrash_…` proves honest cap-exit) | `cargo test --workspace`, every PR |
| **this suite** (new) | **loop behavior end-to-end** on *real* inference + *user-phrased* asks, incl. adversarial failure-mode scenarios | live, on demand / release (this doc) |

It is the **live counterpart** to the mocked `tool_round_cap_tests`: the mock
proves the loop logic deterministically; this proves a real model on real
inference actually triggers and benefits from it. It is **L2/L3 (UAT)** in the
`bat-uat.md` tiering — multi-step, user-phrased — and, like `eval-live`, it is
**not a per-PR gate** (too slow/flaky for a push hook); run it on demand or at
release against a reliable endpoint.

## The technique (how to write one of these)

Each scenario is a small, **disposable** workspace + a **user-phrased** prompt,
run through `newt code` in an isolated sandbox `HOME`, with a **dual assertion**:

1. **Record golden pre-state.** Before the run, capture the workspace's
   observable behavior (run the code, note the bug, the file list) so the
   outcome is verifiable.
2. **Isolated sandbox + default-ish config.** Run in a throwaway `HOME` with a
   minimal `~/.newt/config.toml` (`debug`, `no_splash`, `permissions.preset`),
   so the test exercises the harness **defaults** (e.g. the Phase-27 default-on
   plan ledger for local backends). Per-scenario overrides (e.g.
   `max_tool_rounds`) go in that config.
3. **Pipe a user-phrased prompt + `exit`**, capture the full session with
   `NEWT_DEBUG=1` (per-round token usage, tool calls, compression, loop guards).
4. **Assert two things:**
   - **Outcome** — verify the *workspace result* programmatically (file edited
     correctly? branch created and committed on it? bug fixed? test passes?
     nothing fabricated?). Don't trust the model's self-report.
   - **Behavior** — grep the session log for **tool-loop signals**: round count,
     tool calls, `hallucination(s) corrected` (27.1), dup-guard steer (27.3),
     `plan_set`/`plan_advance` (27.4), and an **honest** cap-exit (27.5 —
     names "failed tool calls / tooling problem", never advises "raise
     `max_tool_rounds`" when the run was thrash-dominated).
5. **Adversarial scenarios deliberately induce the failure modes** — a
   forced repeated dead-tool call (→ dup-guard), an unmet precondition (→ honest
   report vs fabrication), an impossible/churn task under a low `max_tool_rounds`
   (→ honest cap-exit). These are the scenarios the golden-diff cases can't reach.

### Environment notes & gotchas (load-bearing)

- **Endpoint reliability:** prefer **dgx1** (`REDACTED-HOST:11434`) — it is
  reliable under load. **gpu-runner-ollama flakes** on multi-turn / large-`num_ctx`
  sessions (connection drops → timeouts), which silently aborts longer probes.
- **`run_command` is the dead stub** on the crates.io-publishable build
  ("temporarily unavailable in this build") **yet still advertised** — so any
  shell-dependent scenario is really testing *dead-tool handling* (27.3 + the
  honest cap-exit), which is exactly the originally-reported failure
  ("repeated the same dead `run_command` 3×"). On a `--yolo`/real-shell build it
  tests real command failures instead. Either way the loop behavior is the SUT.
- **NEVER `pkill -f <name>` for a proxy/newt** in the runner — the pattern
  matches the runner's own command line and self-kills the shell (exit 144,
  empty output, no artifacts; this cost real debugging time). Kill helpers by
  tracked **PID**; clear ports with **`fuser -k <port>/tcp`** (port-based, no
  self-match).
- Long overflow sessions are slow (a stuck round can burn the whole
  `inference_timeout_secs`); keep it modest (≤120s) and bound each session with
  `timeout`.

## Scenario catalog

| # | Prompt (user-phrased) | Validates | Outcome assertion |
|---|---|---|---|
| S1 | rename a function across a file | edit-tool discovery (27.1) | old name gone, new present, code runs |
| S2 | create branch + switch + add constant + commit on it | **git branch/checkout (27.2)** | current branch = feature branch; commit on it; constant present |
| S3 | fix a bug + add a test | multi-tool (read→diagnose→edit→test) | bug value corrected; test file present and passes |
| S4 | refactor a god function into 3 helpers | multi-step / **plan ledger (27.4)** | ≥4 defs; printed output byte-identical |
| S5 | "make app.py run" — imports a missing module; "don't create files, explain" | **honest behavior under unmet precondition** | does NOT fabricate the module; names the missing dependency |
| S6 | force repeated identical dead `make build` | **dup-guard (27.3)** | identical failed call is short-circuited/steered, not re-run to the cap |
| S7 | dead-shell churn under `max_tool_rounds = 6` | **honest cap-exit (27.5)** | reaches cap; message names tooling/failed-calls; does NOT advise "raise the cap" |

S1–S4 are capability/correctness UAT; **S5–S7 are the adversarial failure-mode
UAT** that exercise the loop guards the golden-diff cases never trigger.

## Prerequisite — real shell (`just install-real`)

`run_command` (the confined brush shell) is a **fail-closed stub** on the
default/release build — the crates.io-safe build returns *"temporarily unavailable
in this build"* yet still advertises the tool. So **any scenario that exercises
`run_command` (S6/S7, and `run`-shaped prompts) must first install the real
shell**, or it is testing dead-tool handling rather than the shell:

```bash
just install-real     # swaps in the real brush OCAP shell (clean + shell-real + install)
# … run the UAT against $HOME/bin/newt …
just shell-stub       # restore the stub before committing (the CI shell-check guard rejects the git dep)
```

This is a **standing stopgap until reubeno/brush#1184** (the upstream
`CommandInterceptor`) is accepted; then the real shell is the default and
`install-real` is unnecessary. The non-shell scenarios (S1–S5) run fine on the
stub build, so a capability-only pass needs no `install-real`.

## Running it

```bash
cargo build --release --bin newt
docs/testing/scripts/uat_tool_loop.sh                                  # dgx1, qwen3-coder:30b
docs/testing/scripts/uat_tool_loop.sh http://REDACTED-HOST:11434 qwen3-coder:30b
docs/testing/scripts/uat_tool_loop.sh https://REDACTED-HOST llama3.1:8b   # weaker model; expect more flakiness
```

It prints a per-scenario `OUTCOME | BEHAVIOR` line and the aggregate tool-loop
signal counts. Capture stdout into a dated `docs/testing/results/<…>.md`
following the existing results template (TL;DR, environment, per-scenario table,
honest caveats), and note the model + endpoint + commit.

## Reading the results honestly

- An **outcome pass** means the task landed; a **behavior signal** means the
  loop guard *fired*. A guard that **didn't fire** is not a failure — it means
  the scenario didn't trigger it (e.g. a clean run needs no dup-guard). Say so;
  don't infer a guard works from a clean run that never stressed it.
- **Model strength gates which guards you can observe (important).** The
  dup-guard (S6, 27.3) and honest cap-exit (S7, 27.5) are **safety nets for
  *weak* models that loop/thrash**. A *capable* model (e.g. qwen3-coder:30b)
  reads the dead-tool "unavailable" message and **adapts after one call** — it
  never re-issues the identical call within a turn, so the short-circuit has
  nothing to fire on, and it abandons a hopeless task before the cap. To
  observe 27.3/27.5 *live* you need a model weak enough to actually loop (the
  one from the original forensic session); the **deterministic** guarantee is the
  mocked `uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit` test
  (`tool_round_cap_tests`), which scripts exactly that thrash. Use the live S6/S7
  runs to confirm a strong model *doesn't* loop, and the mock to confirm the
  guard *catches* one that does.
- **Model-workflow / judgment gaps are often promptable, not harness bugs.**
  e.g. "created the branch but committed on `master`" and "fabricated the missing
  module" both vanished with a sharper prompt (explicit "switch to it and commit
  ON the branch"; "do NOT create files, explain"). Re-run with a sharpened prompt
  before filing a harness issue — and treat persistently-needed sharpening as a
  system-prompt/`--coder` improvement, not a per-task fix.
- Single live runs are **noisy** (model nondeterminism, endpoint variance). For
  a number you'd cite, run N≥5 and report the distribution — see the offload
  statistical arm in `docs/testing/results/`.
- This suite validates **behavior**; the **logic** is pinned deterministically
  by `tool_round_cap_tests` and the feature unit tests. Cite both.
