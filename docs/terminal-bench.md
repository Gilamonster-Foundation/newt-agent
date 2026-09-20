# Terminal-Bench scoreboard

Measured on [Terminal-Bench](https://github.com/harbor-framework/terminal-bench)
via `newt headless` and the Harbor adapter, confined (**OCAP on**) and unconfined
(**OCAP off**). Each lane is a per-model monotonic ratchet: a score never goes
down across releases.

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

Evidence, provenance, rejected runs, and the scoring rules live in
[`gilamonster-bench`](https://github.com/Gilamonster-Foundation/gilamonster-bench/tree/main/scoreboard),
an instrument with no dependency on Newt. If the ruler shipped with the thing
it measures, one commit could move both at once.

How the runs are produced: [`scripts/eval/harbor/README.md`](../scripts/eval/harbor/README.md).
The block between the markers is rewritten by `just bench-publish`
(`scripts/eval/bench_scoreboard.py render`); edit the manifest, not the table.


## Headless policy identity

New headless traces declare observability contract version `3`. Their existing
`agent_version` field carries the opaque `VERSION_WITH_COMMIT` build string,
including any source suffix or dirty marker. Preserve it exactly: package
semver alone cannot distinguish different implementations of the same dials.
An independent candidate binary digest is still needed to identify the exact
executable; a build string is not binary authentication.

The existing `config_digest` field is a `content-addressable` structured
`ContentId` minted from canonical DAG-CBOR over the **complete emitted
`effective_config` JSON value**. Unknown nested members, array order and the
difference between absence and explicit null participate. JSON whitespace and
object key order do not. There is no selected-key hash or default insertion.
Encoding failure refuses the record before the events file is opened, rather
than emitting a partial identity.

The configuration retains projected `cognition` and separately records captured
`semantic_cognition`; a captured null is not proof of an operator-off selection.
Admitted Responses effort, instantiated verification and allowance, numeric
initiative threshold, and existing output/run/round limits describe the policy
actually supplied to the turn. The same verification policy remains in the
existing receipt; completion and verification results remain execution evidence.
A missing outcome does not manufacture instantiated-policy evidence. This
contract does not claim that future cognition techniques have been implemented.
The `wire_api` field describes the configured typed dispatch selection, even if
no request occurs; it does not manufacture request or technique evidence.

Smart configuration contains the immutable launch manifest, including starting
CID and session configuration. Its final execution head remains available in
`solve_result.smart_harness.head`. This is an explicit v3 correction: legacy
configuration also contained that runtime head. Changing only the final head,
answer, timing or build string cannot change configuration identity; changing a
captured starting CID or configuration can.

Historical v1/v2 records retain their original field meanings and receipts.
Missing build/config identity is unknown, not equivalent to current defaults.
Do not pool runs by dial labels when build or configuration identity differs or
is unknown; compare model, fixture and contract identity as well. The independent
consumer verifies claimed Newt v3 identity before binding a result. Keep the
original readable events file beside the bound row: the row's digest cannot
reconstruct the full configuration. Retention is caller-managed, and matrix
attempt-count summaries are not policy-qualified rows. Identity proves which
configuration was described, not model quality or provider authenticity.
