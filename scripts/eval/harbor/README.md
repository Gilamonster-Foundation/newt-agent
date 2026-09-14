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

## pi and Codex on the same endpoint

[`pi_local.py`](pi_local.py) and [`codex_local.py`](codex_local.py) subclass
Harbor's built-in `pi` and `codex` agents (#2318). Each writes a provider entry
for a local OpenAI-compatible endpoint inside the task container, then runs the
stock agent: pi gets `~/.pi/agent/models.json`, Codex gets
`model_providers.local` in `$CODEX_HOME/config.toml`. `PiLocal` also installs the
maintained `@earendil-works/pi-coding-agent`; Harbor 0.20 installs the deprecated
package name, which is frozen at 0.73.1.

```bash
TB_LOCAL_BASE_URL=http://<endpoint>/v1 \
TB_LOCAL_CONTEXT_WINDOW=<ctx-size as served> \
PYTHONPATH=scripts/eval/harbor \
harbor run --config <job.json>   # agents: pi_local:PiLocal or codex_local:CodexLocal
```

Set the job's `model_name` to `local/<model id as served>`. Export the two
variables into Harbor's own environment. Passing them with `--ae` records the
endpoint in the job config. Harbor still grades the task and records token usage
from each harness's log.

## Harness × model campaign

[`tb-campaign.sh`](tb-campaign.sh) runs newt, pi and Codex for each model in a
roster ([`tb-roster-baseline.txt`](tb-roster-baseline.txt)). Every harness gets
the same task set, trial count, served context window and agent timeout, and
Harbor's verifier grades every trial. Models are processed one at a time. For
each model, the script loads it and checks that the served `--ctx-size` matches
`TB_CTX_SIZE` before each cell. It also waits for the model's slot to go idle,
so a generation left over from a killed trial cannot overlap the next cell.

```bash
NEWT_BENCH_BIN=/path/to/bookworm-built/newt \
NEWT_BENCH_PROFILE_TEMPLATE=~/.newt/bench/<profile>.toml \
TB_CTX_SIZE=131072 TB_TRIALS=3 \
bash scripts/eval/harbor/tb-campaign.sh \
  scripts/eval/harbor/tb-roster-baseline.txt scripts/eval/harbor/smart-ab-8.json <campaign>
python3 scripts/eval/harbor/tb_campaign.py table /var/tmp/tbench-harbor/<campaign>
```

The profile's `endpoint` serves all three harnesses. A campaign directory holds
one Harbor job per cell, plus these records:
- `trials.jsonl` has one row for every trial directory Harbor created: reward,
  grading state, exception, claim, tokens and agent seconds.
- `cells.jsonl` binds each cell to the served model, the context window as
  served, the engine build, the harness version, the newt binary digest, the
  task-set digest and the expected trial count.

Rerunning the command skips recorded cells.

Reading the table:
- **Claimed done** comes from each harness's own log. newt's claim is the
  contract `outcome: completed`, pi's is a final `stopReason: stop`, and Codex's
  is `turn.completed`. When no claim can be read, it counts as *unrecoverable*,
  not as "did not claim". A trial killed by Harbor's timeout counts as "did not
  claim".
- **False completion** means the harness claimed done and Harbor did not
  resolve the trial. **False incomplete** is the reverse.
- **Tokens** for pi and Codex are Harbor's `agent_result`, parsed from the
  harness log. newt's contract emits output tokens only, so newt's input tokens
  are missing, not zero.
- **Sandbox and round caps:** newt runs `--unsafe-host-exec` because pi and Codex
  run unsandboxed. newt stops at `NEWT_BENCH_MAX_ROUNDS` (40), while pi and Codex
  have no round cap. The agent timeout is the only limit all three share.
- **Wilson interval:** computed over graded trials. Coverage (expected, observed
  and graded) is shown next to it.

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
