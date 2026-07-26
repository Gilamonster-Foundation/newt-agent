# Practical live regression suite (UAT)

A **live**, hard-failing User-Acceptance suite that drives the real `newt`
harness with user-phrased coding prompts on disposable workspaces against a
real LLM, and asserts both *task correctness* and *tool-loop / evidence-path
behavior*.

**Runner:** [`scripts/uat_tool_loop.sh`](./scripts/uat_tool_loop.sh).
**CI:** [`.github/workflows/practical-regression.yml`](../../.github/workflows/practical-regression.yml).
**Local:** `just practical-regression`.
**Issue filer:** [`scripts/eval/file-practical-regressions.sh`](../../scripts/eval/file-practical-regressions.sh).

## Where it sits (relative to [`bat-uat.md`](./bat-uat.md))

| Mechanism | What it asserts | Where |
|---|---|---|
| `newt-eval` golden-diff cases | **diff correctness** on fixed cases | mock per-PR (`mock_e2e`), live weekly (`eval-live.yml`) |
| `bat_largest_files` (mocked BAT) | line-count / largest-files **composition** vs a scripted model | `cargo test`, every PR |
| `tool_round_cap_tests` (mocked) | **loop hardening** vs a scripted model | `cargo test`, every PR |
| **this suite** (live) | **practical tasks end-to-end** on real inference + user-phrased asks | post-merge smoke, nightly full, manual |

It is the **live counterpart** to the mocked BATs: mocks prove the harness path
deterministically; this proves a real model on real inference still discovers
and uses it. **Not a per-PR gate** — too slow/flaky for a push hook.

## Contract

- Status per scenario: `PASS` | `FAIL` | `INFRA`.
- Aggregate exit: `0` = all PASS, `1` = at least one FAIL (behavioral),
  `2` = INFRA / hard error (binary missing, endpoint down).
- A FAIL means the harness is considered broken for that practical task
  (e.g. the model cannot count lines via `find sort=lines`).
- INFRA (unreachable endpoint, missing provider) makes the workflow red but
  does **not** open a model-regression issue.
- No hosts, keys, or home-network details are committed. The runner's local
  `~/.newt/config.toml` names a provider (`dgx1-llama`); CI sets
  `NEWT_PROVIDER` / `NEWT_DGX_MODEL` only.

## Suites

### `smoke` (late post-merge, after successful CI on `main`)

| Case | Prompt (summary) | Asserts |
|---|---|---|
| `line-count` | "show me the 10 code files with the highest line counts in this repository?" | `find … sort=lines`; descending 120/40/2; no bytesize fallback; no empty response |
| `rename` | Rename `area_of_circle` → `circle_area` | old gone, new present, code runs |
| `bugfix` | Fix `apply_discount` + add test | returns 90.0; test file present |

### `full` (nightly 03:00 Eastern + manual)

Smoke plus:

| Case | Asserts |
|---|---|
| `branch` | feature branch current; constant present; commit on branch |
| `refactor` | ≥4 defs; printed output still valid |
| `deadend` | does not fabricate missing module; names the dependency |
| `duploop` | repeated dead `make build` steered or abandoned (≤3 calls) |
| `capexit` | honest exit; does **not** advise raising `max_tool_rounds` |

The line-count fixture deliberately inverts byte vs line order (`fat.rs` is
largest by bytes, `tall.rs` by lines) so a `sort=size` fallback fails the gate.

## Running it

```bash
# Self-test (no inference — CI-safe):
just practical-regression-self-test
# or:
docs/testing/scripts/uat_tool_loop.sh --self-test
scripts/eval/file-practical-regressions.sh --self-test

# Full suite against runner-local DGX/Nemotron provider:
just practical-regression

# Post-merge smoke only:
just practical-regression smoke

# One scenario (fast reproduction of the line-count regression):
just practical-regression smoke line-count

# Pin provider/model explicitly:
just practical-regression full '' dgx1-llama NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-GGUF_UD-Q4_K_XL

# Legacy positional host/model (sets NEWT_DGX_OLLAMA_URL):
docs/testing/scripts/uat_tool_loop.sh --suite smoke --case line-count \
  --provider dgx1-llama --model NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-GGUF_UD-Q4_K_XL
```

### Runner-local config

On the `gnuc` self-hosted runner (or your laptop), copy
[`practical-regression.local.example`](./practical-regression.local.example)
into `~/.newt/config.toml` and fill in the real DGX endpoint. The committed
example uses placeholders only.

## CI/CD wiring

| Trigger | Suite | Issues |
|---|---|---|
| `workflow_run` after successful `CI` on `main` | `smoke` | no (signal only) |
| Nightly 03:00 `America/New_York` | `full` | yes — deduped `practical-regression: <case>` |
| `workflow_dispatch` | chosen suite/case | yes (unless dry-run) |

Artifacts per run: `results.tsv`, `summary.md`, per-scenario `sessions/*.log`.

## Prerequisite — real shell (`just install-real`)

`duploop` / `capexit` exercise `run_command`. On the crates.io-safe stub build
that tool is advertised but dead — those scenarios then test dead-tool handling
(which is still useful). For a real shell:

```bash
just install-real     # real brush OCAP shell
# … run the UAT against $HOME/bin/newt …
just shell-stub       # restore before committing
```

Smoke (`line-count` / `rename` / `bugfix`) does **not** need `install-real`.

## Reading results

- **PASS** — outcome + required behavior signals held.
- **FAIL** — model reached, but wrong tool path / wrong workspace result / empty
  response after tools. File a harness bug (nightly does this automatically).
- **INFRA** — endpoint/provider unreachable; fix the runner config, don't blame
  the model.
- Single live runs are noisy. Nightly + post-merge smoke together give the
  durable signal; the mocked BAT remains the per-PR proof of the harness path.
