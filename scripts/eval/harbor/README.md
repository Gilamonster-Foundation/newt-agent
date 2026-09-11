# newt ↔ Harbor / Terminal-Bench adapter (WS3, #1419)

`newt_agent.py` is a Harbor **installed-agent** adapter that drives `newt solve`
on real, containerized Terminal-Bench tasks — the entry to the release-champion
ceremony's reproducible lane.

It **injects** the locally-built `newt` binary + a pinned backend profile into
each task container (newt has no public package yet), then runs `newt solve`.
**The injected binary must target the oldest glibc among the task images** —
a host-built binary on Ubuntu 24.04 needs `GLIBC_2.39`, and Debian-bookworm
based tasks ship 2.36 and refuse it at exec, silently scoring 0. Build it in
`rust:<msrv>-bookworm` (see `smart-ab.sh`, which refuses a too-new binary)
headless in `/app`, writing the trace to `/logs/agent/newt-events.jsonl`.
Inference reaches the pinned endpoint from *inside* the container.

## Run

```bash
harbor download terminal-bench -o /var/tmp/tbench-tasks     # pull the task set

NEWT_BENCH_BIN=~/bin/newt \
NEWT_BENCH_PROFILE=/path/to/bench.toml \   # LOCAL, host-secret: endpoint + model
NEWT_BENCH_TENACITY=insistent \            # optional dial
PYTHONPATH=scripts/eval/harbor \
harbor run --config scripts/eval/harbor/newt-job.example.json
```

`bench.toml` is a `[[backends]]` profile (see `../tbench-profile.example.toml`).
Host secrecy (RATCHET.md): the endpoint lives ONLY in that local file.

## Proven

First-light 2026-07-28: `regex-log` ran end-to-end in a container — newt 0.7.5
injected + executed (glibc-compatible), a ~2.3-min qwen3-coder@dgx1 solve,
clean trace (`status:completed, error:null`), verifier reward `0`. A real,
trustworthy Terminal-Bench data point.
