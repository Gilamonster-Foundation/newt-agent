# newt ↔ Harbor / Terminal-Bench adapter (WS3, #1419)

[`newt_agent.py`](newt_agent.py) injects a branch-built `newt` binary and a local
backend profile into each Harbor task container, then runs `newt solve`.
It uses Harbor's `task_env_config.workdir`, otherwise the container's `pwd`,
falling back to `/app` if neither supplies a directory. The trace goes to
`/logs/agent/newt-events.jsonl`; smart mode also stores its frame under
`/logs/agent/frame`. The configured inference endpoint must be reachable from
inside the container.

## Run

```bash
harbor download terminal-bench -o /var/tmp/tbench-tasks

NEWT_BENCH_BIN=/path/to/branch-built/newt \
NEWT_BENCH_PROFILE=/path/to/bench.toml \
NEWT_BENCH_TENACITY=insistent \
PYTHONPATH=scripts/eval/harbor \
harbor run --config scripts/eval/harbor/newt-job.example.json
```

Run from the repository root with Harbor installed. Adapt the job's dataset
path, `jobs_dir`, and model label to your setup. The injected profile determines
the actual endpoint and model; keep Harbor's `model_name` label consistent.
Use a local, uncommitted copy of the
[backend profile example](../tbench-profile.example.toml) for endpoint details
and credentials. `NEWT_BENCH_TENACITY` is optional.

## Smart-harness comparison

[`smart-ab.sh`](smart-ab.sh) runs the plain arm, then the smart arm, using the
same binary and supplied job configuration:

```bash
NEWT_BENCH_BIN=/path/to/branch-built/newt \
NEWT_BENCH_PROFILES=qwen \
bash scripts/eval/harbor/smart-ab.sh \
  scripts/eval/harbor/smart-ab-8.json experiment-001
```

Prepare `~/.newt/bench/<prefix>-plain.toml` and
`~/.newt/bench/<prefix>-smart.toml`; the prefix defaults to `qwen`. The plain
profile must omit `[smart_harness]` or set `enabled = false`: omitting the CLI
flag does not override an enabled profile. The smart profile supplies the
[smart-harness configuration](../../../docs/guide/smart-harness.md), and the
runner adds `--smart-harness --frame-dir /logs/agent/frame`. Keep the primary
model, endpoint, and shared settings equivalent across profiles; the runner
checks that the files exist, but does not compare their contents.

The runner sets `PYTHONPATH`, preserves task environments with `--no-delete`,
and reads results from the supplied job's `jobs_dir`. Each arm is named
`smart-off-<label>` or `smart-on-<label>`; use a distinct label for each experiment.

| Variable | Default | Effect |
|---|---|---|
| `NEWT_BENCH_GLIBC_FLOOR` | `2.36` | Rejects binaries requiring a newer glibc, using `objdump`. |
| `NEWT_BENCH_CONCURRENCY` | `1` | Harbor's concurrent trial count per arm. |
| `NEWT_BENCH_TIMEOUT_MULT` | `3` | Multiplies Harbor's agent timeout. |

Build the binary against the oldest task image's glibc, for example in a
compatible `rust:<msrv>-bookworm` image. Change the floor only after checking
the selected images. Adjust concurrency and timeouts to the inference service
and task budgets, keeping them identical across arms.

## Tasks and interpretation

The [12-task candidate set](smart-ab-12.json) produced eight successful oracle
runs in the recorded 2026-09-11 check. The [eight-task subset](smart-ab-8.json)
contains those tasks; [the exclusion notes](smart-ab-8.EXCLUDED.md) record the
four reference-solution failures. Oracle results and the glibc check validate
reference-solution viability and one binary compatibility requirement. They do
not establish model quality or a smart-harness improvement.

[`cross-tab.py`](cross-tab.py) reports Harbor resolutions, newt's completion
claims, and **false completions**: newt reported `completed` while Harbor's
verifier reported failure. It combines the contract's outcome with the
`solve_result` terminal reason. Inspect missing records and verifier errors
alongside these counts before drawing conclusions from an arm comparison.

## Historical baseline

On 2026-07-28, newt 0.7.5 ran `regex-log` in a container and reported
`status:completed, error:null`, while the verifier awarded `0`. This historical
adapter run demonstrates why a completion claim needs an independent verdict;
it is not a result for the current smart-harness comparison.
