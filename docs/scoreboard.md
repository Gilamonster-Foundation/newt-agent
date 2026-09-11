# Terminal-Bench scoreboard — full table

Every model on the roster: measured on both lanes, half-measured
(`_pending_`), or not yet run (`_queued_`). The README carries only the best
completed rows; this is the whole matrix. Source of truth is
[`scripts/eval/bench-results.jsonl`](../scripts/eval/bench-results.jsonl);
`just bench-publish` rewrites both tables from it. Methodology, provenance,
and rejected runs: [gilamonster-bench](https://github.com/Gilamonster-Foundation/gilamonster-bench/tree/main/scoreboard);
per-release snapshots: [`releases/`](./releases/).

<!-- BENCH-SCOREBOARD:START -->
_Per-model Terminal-Bench champions, **OCAP off vs on**. Each lane is a monotonic ratchet (a score never goes down). 0.7.6 establishes the honesty-classified, digest-pinned confined (OCAP-on) baseline; OCAP-on within reach of OCAP-off (parity) is pursued forward via pre-granted permissions, not gated here. Auto-generated; do not edit by hand._

| Model | OCAP off | OCAP on |
|-------|----------|---------|
| `deepseek-v4-pro`<br><sub>deepseek · tb-30 · ctx 65536 · v0.8.0 · 2026-08-06</sub> | 56.7% (17/30) | 50.0% (15/30) |
| `nemotron-3-super`<br><sub>nemotron · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 36.7% (11/30) | 26.7% (8/30) |
| `ornith-1.0-35b-q8`<br><sub>ornith · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | _pending_ | 36.7% (11/30) |
| `qwen3.6_35b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 20.0% (6/30) | 26.7% (8/30) |
| `o4-mini`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 13.3% (4/30) | 16.7% (5/30) |
| `qwen3-coder_30b`<br><sub>qwen · tb-30 · ctx 65536 · v0.7.6 · 2026-07-29</sub> | 10.0% (3/30) | 13.3% (4/30) |
| `gpt-oss_120b`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 10.0% (3/30) | 10.0% (3/30) |
| `kimi-linear_48b`<br><sub>kimi · tb-30 · ctx 65536 · v0.7.6 · 2026-07-31</sub> | _pending_ | 10.0% (3/30) |
| `nemotron-3-nano_30b`<br><sub>nemotron · tb-30 · ctx 65536 · v0.7.5 · 2026-07-29</sub> | 6.7% (2/30) | _pending_ |
| `glm-4.7-flash`<br><sub>glm · tb-30 · ctx 65536 · v0.7.6 · 2026-07-31</sub> | _pending_ | 3.3% (1/30) |
| `gpt-4.1-mini`<br><sub>openai · tb-30 · ctx 65536 · v0.8.0 · 2026-08-05</sub> | 0.0% (0/30) | 3.3% (1/30) |
| `kimi-k2.7-code`<br><sub>kimi · queued</sub> | _queued_ | _queued_ |
| `nemotron-3-ultra`<br><sub>nemotron · queued</sub> | _queued_ | _queued_ |
| `ornith-1.0-397b-iq1_m`<br><sub>ornith · queued</sub> | _queued_ | _queued_ |

<!-- BENCH-SCOREBOARD:END -->
