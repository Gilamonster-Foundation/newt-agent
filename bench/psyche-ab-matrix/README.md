# Psyche × OCAP A/B matrix (ornith-1.0-35b-q8)

A portable, reproducible bench that measures how newt's **psyche** dials and the
**OCAP** lane affect a headless `newt solve` run, on the `ornith-1.0-35b-q8`
model served by **dgx1**. Built for the `feat/psyche` branch; designed to run
unattended on **gnuc** (or any host that can reach dgx1).

## What it does

For every **posture × OCAP** cell it runs `newt solve` over a set of
self-verifying tasks, records the result + harness metrics, and renders a
matrix report.

- **Postures** (psyche axis) — set via global flags / env only, no per-cell config edits:
  | posture | how it's set | meaning |
  |---|---|---|
  | `baseline` | (nothing) | cognition off · tenacity standard · no crew |
  | `tenacity` | `--tenacity relentless` | max loop push |
  | `crew` | `NEWT_TEAM=1` | multi-agent crew on |
  | `obsessive` | `--obsessive` | contemplating + relentless + crew |
- **OCAP** axis:
  | mode | how it's set | meaning |
  |---|---|---|
  | `off` | `solve --non-interactive true` | yolo / full-access, no prompts |
  | `on` | `NEWT_BENCH_OCAP=on` | confined, workspace-fenced writes |

> **ornith is a chat_completions backend**, so the cognition dial
> (`reasoning.effort`, a Responses-API concept) does **not** wire through — the
> axes that actually move ornith are **tenacity + crew**. The matrix still runs
> all four postures so the null-effect of cognition is recorded, not assumed.
> ornith is also a *heavy* reasoner (~6k tokens/turn even on trivial prompts);
> `MAX_ROUNDS` and `TASK_TIMEOUT` are sized for that.

## Prerequisites

1. A `newt` binary built from **`feat/psyche`** (`cargo build --release -p newt-agent --bin newt`).
2. Network reach to the ornith endpoint. dgx1 serves an OpenAI-compatible
   llama.cpp multiplexer on **:8080**:
   - **Tailscale (works from anywhere on the tailnet, incl. NUC01 + gnuc):** `http://100.113.207.102:8080`
   - **On the lab LAN (gnuc):** `http://dgx1.home.lan:8080` or `http://192.168.0.103:8080`
   `python3` + `curl` on PATH.

## Run it (gnuc)

```bash
cd bench/psyche-ab-matrix
cargo build --release -p newt-agent --bin newt        # if not already built
# LAN endpoint is lower-latency from gnuc; Tailscale is the default and also works:
ORNITH_ENDPOINT=http://192.168.0.103:8080 ./run-matrix.sh
```

Everything is env-overridable (defaults in `run-matrix.sh`):

```bash
NEWT=/path/to/newt \
ORNITH_ENDPOINT=http://100.113.207.102:8080 \
ORNITH_MODEL=ornith-1.0-35b-q8 \
TASKS_DIR=./tasks \
POSTURES="baseline tenacity crew obsessive" \
OCAP_MODES="off on" \
MAX_ROUNDS=15 TASK_TIMEOUT=600 \
OUT=./runs/mystamp \
./run-matrix.sh
```

The runner fails fast if the endpoint isn't reachable (a `/v1/models` 200 check)
so a whole matrix never burns on a dead backend.

## Output

Under `runs/<stamp>/`:
- **`matrix.md`** — the posture × OCAP grid (pass-rate + avg tokens/wall per cell) + per-cell task detail.
- **`results.csv`** — one row per (posture, ocap, task): verify pass/fail, status, tool_calls, write_calls, total_tokens, wall_secs, solve_rc.
- per-cell `.trace` + `.jsonl` (the raw solve event stream) for debugging.

## Task set

Small, self-verifying, fast (`tasks/<name>/{instruction.txt,setup.sh,verify.sh}`):
`write-greeting` (inference + a write), `edit-version` (find + edit an existing
file), `fix-typo` (locate + correct). Add tasks by dropping in a new dir with the
same three files (`verify.sh` exits 0 = pass). To point the matrix at the full
terminal-bench corpus instead, set `TASKS_DIR` to a directory of tb tasks laid
out the same way.

## Reproducibility notes

- No `~/.newt` dependency: the runner renders its own `ornith.toml` from
  `ORNITH_ENDPOINT`/`ORNITH_MODEL` into the run dir and pins it with `--config`.
- All psyche/OCAP knobs are CLI flags + env — nothing host-specific.
- `newt --config` is honored by the `solve` path (`Config::load`), so the matrix
  is unaffected by whatever is in the operator's real config.
