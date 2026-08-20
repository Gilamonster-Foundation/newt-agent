# BAT / UAT acceptance tiers

newt's tests run in tiers (see `CLAUDE.md` → "Testing strategy"). This doc
covers the two acceptance tiers between the fully-mocked unit tier and pure
release gates: **BAT** (Basic Acceptance) and **UAT** (User Acceptance), and how
they're wired into CI/CD — including the live run against a **hosted model**
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
- **newt-web browser acceptance** — a Playwright/Chromium harness starts the
  real web binary and a simulated Ollama boundary. BAT verifies the canonical
  Markdown/progressive-enhancement contract on every newt-web PR and `main`;
  phone-sized UAT drives a complete prompt/reply/Mermaid flow on `release/**`
  and manual dispatch. Run it with `npm run test:bat` or `npm run test:uat` from
  `newt-web/`.

The Rust/mock tiers are fast, deterministic, and parallel-safe — no real
network, fs, subprocess, or wall-clock. The browser BAT/UAT is the deliberate
grounding add-on: it is still deterministic and uses no external network, but
runs serially against real loopback processes, a temporary store, and Chromium.

### 2. Live (a real hosted model) — weekly + release, in `eval-hosted.yml`

The expensive end of the pyramid. `.github/workflows/eval-hosted.yml` replays
the *same* eval cases in **live mode** against a real model served over an
OpenAI-compatible endpoint:

- **Triggers:** weekly cron (Mondays 07:17 UTC), `workflow_dispatch` (with an
  optional model / difficulty override), and pushes to `release/**` (a release
  gate). Never per-PR — too slow and too costly for a push gate.
- **Runner:** a GitHub-hosted `ubuntu-latest`. No owned hardware.
- **Configuration:** the endpoint is the `EVAL_BASE_URL` repo variable, the
  model is the `EVAL_MODEL` repo variable (or the dispatch input), and the
  bearer token is the `EVAL_API_KEY` secret. Per RATCHET.md no host or key
  appears in the workflow file; the model is named only.
- **Unconfigured is a skip, not a failure.** A preflight job checks for the
  variable and the secret and skips the eval when either is missing, so a fork
  — or this repo before the secret exists — does not get a red schedule.
- **Serial by design:** the eval driver runs cases one at a time and the worker
  talks to a real model, satisfying the "real-resource tests run
  single-threaded" rule.
- **HOOK PARITY EXCEPTION:** like the `windows` job and `mesh-integration`, this
  is CI-only; the local equivalent is `just eval-live`.

#### What this replaced

Until 2026-08-20 this tier ran as `eval-live.yml` (weekly) and
`nightly-eval.yml` (nightly) on a **self-hosted runner** beside a local Ollama,
which is being decommissioned. The nightly had not passed in its last 40 runs,
so it graded nothing while still reporting red every morning. The eval machinery
under `scripts/eval/` is unchanged and still runs by hand.

Two behavioral differences worth knowing:

- **No `--coder` flag.** It existed because small local coder models cannot
  reliably fabricate valid diff hunk headers, so prompts were routed through the
  newt-coder whole-file plugin. A hosted frontier model does not need it, and
  leaving it on would grade a different code path than users exercise.
- **No informational matrix leg.** The old weekly tracked a general-purpose
  local model non-gating alongside a gating coder model. With one hosted model
  the run is simply gating. Add a matrix with per-leg `continue-on-error` if a
  tracked-only model becomes useful again.

## Running it by hand

```bash
just eval                                   # mock-mode cases, local Ollama
just eval-live                              # live, default model @ localhost
just eval-live qwen2.5-coder:7b             # pick a model
just eval-live llama3.1:8b http://gpu-host:11434 --difficulty L2,L3   # UAT only
```

`--difficulty L1` selects the BAT (smoke) cases; `L2,L3` selects UAT.

## When you change CI

Per `CLAUDE.md` "Push Hook Governance": the per-PR jobs in `ci.yml` are mirrored
by `.githooks/pre-push` (`just check` + `just cov-ci`). `eval-hosted.yml` is a
documented parity *exception* (the live tier can't run in a push gate). When you
edit either CI file, re-check the hook/pipeline cross-reference comments.
