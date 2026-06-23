# BAT / UAT acceptance tiers

newt's tests run in tiers (see `CLAUDE.md` → "Testing strategy"). This doc
covers the two acceptance tiers between the fully-mocked unit tier and pure
release gates: **BAT** (Basic Acceptance) and **UAT** (User Acceptance), and how
they're wired into CI/CD — including the live run against the **gnuc** home box
with real test LLMs.

## The two acceptance tiers

| | BAT — Basic Acceptance | UAT — User Acceptance |
|---|---|---|
| Question | "Does the wired-up system accept the basic flows?" | "Does an end-user scenario work the way a user would phrase it?" |
| Shape | smoke / contract-level | multi-step / cross-domain, user-phrased |
| eval cases | L1 (`difficulty = "L1"`) | L2 / L3 |

Both run against a **simulated integration environment** (mocked/stubbed
externals) on every PR, and against **real test LLMs** on a schedule.

## Where they run

### 1. Simulated (mocked) — every PR, in `ci.yml`

- **Coder eval, mock mode** — `newt-eval`'s golden-diff cases replayed against a
  mock Ollama. Runs as part of `cargo test --workspace` (the `cargo test` job in
  `.github/workflows/ci.yml`); the dedicated entry point is
  `cargo test -p newt-eval --test mock_e2e`. Authoring: `newt-eval/cases/CASE_AUTHORING.md`.
- **Agentic-loop acceptance** — wiremock-scripted scenarios that replay
  real-world loop behavior against a *simulated* model, in
  `newt-core/src/agentic/mod.rs` (`tool_round_cap_tests`). Example: the Phase 27
  `uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit` test scripts a model
  that thrashes (a distinct failing tool call every round + a failing summary)
  and asserts the cap-exit is honest. These are the durable regression guard for
  the loop hardening and gate every PR.

These are fast, deterministic, and parallel-safe — no real network, fs,
subprocess, or wall-clock.

### 2. Live (real LLMs on gnuc) — weekly + release, in `eval-live.yml`

The expensive end of the pyramid. `.github/workflows/eval-live.yml` replays the
*same* eval cases in **live mode** against real models served by Ollama on the
`gnuc` home box:

- **Triggers:** weekly cron (Mondays 07:17 UTC), `workflow_dispatch` (with an
  optional single-model / difficulty override), and pushes to `release/**` (a
  release gate). Never per-PR — too slow/flaky for a push gate.
- **Runner:** a self-hosted GitHub Actions runner labelled `gnuc`. The matrix
  models must be pulled there (`ollama pull <model>`).
- **Serial by design:** the eval driver runs cases one at a time (no parallel
  flag) and the worker talks to a real model — satisfying the
  "real-resource tests run single-threaded" rule.
- **HOOK PARITY EXCEPTION:** like the `windows` job and `mesh-integration`, this
  is CI-only (it needs real models); the local equivalent is `just eval-live`.

#### Setting up the gnuc runner (one-time)

1. On gnuc, register a self-hosted runner with the label `gnuc`
   (GitHub → repo Settings → Actions → Runners → New self-hosted runner).
2. Ensure Ollama is serving and the matrix models are pulled:
   `ollama pull qwen2.5-coder:7b && ollama pull llama3.1:8b`.
3. (Optional) set a repo variable `GNUC_OLLAMA_HOST` if Ollama isn't on the
   runner's `http://127.0.0.1:11434` (e.g. point it at `dgx1.home.lab:11434`).
4. Edit the `matrix.model` list in `eval-live.yml` to track your rig.

## Running it by hand

```bash
just eval                                   # mock-mode cases, local Ollama
just eval-live                              # live, default test model @ gnuc
just eval-live qwen2.5-coder:7b             # pick a model
just eval-live llama3.1:8b http://dgx1.home.lab:11434 --difficulty L2,L3   # UAT only
```

`--difficulty L1` selects the BAT (smoke) cases; `L2,L3` selects UAT.

## When you change CI

Per `CLAUDE.md` "Push Hook Governance": the per-PR jobs in `ci.yml` are mirrored
by `.githooks/pre-push` (`just check` + `just cov-ci`). `eval-live.yml` is a
documented parity *exception* (the live tier can't run in a push gate). When you
edit either CI file, re-check the hook/pipeline cross-reference comments.
