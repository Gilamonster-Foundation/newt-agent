# Smart-harness classifier comparison

`smart_harness_cases.json` contains eight curated regression examples: answers,
promises, operator questions, a Spanish question, a completion induced by a
nudge, an answer after a nudge, and an instruction embedded in untrusted evidence.
These are synthetic examples, including two bundled baseline prototypes; they
are not a held-out corpus or a representative sample of production traffic.

Run the same production auxiliary builder, evidence prompt and strict verdict
parser against an already installed model, without downloads:

```sh
NEWT_EMBEDDED_DEVICE=cuda cargo run -p newt-inference --release --features embedded \
  --example smart_harness_eval -- /path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf report.json
```

The deliberately conflicting environment grounds constructor-enforced CPU
placement. `tokenizer.json` must be beside the GGUF. Use a release build for
meaningful timing; debug Candle execution is substantially slower. The run
retains the configured 30-second per-call deadline and overrides output to 16
tokens for the one-string classification task. All settings, raw outputs,
failures, latency, confusion counts, model/tokenizer raw CIDs and the fixture
CID are recorded. The report's identity uses the content-addressable crate.

Keep the original report outside the repository, then produce a portable copy:

```sh
cargo run -p newt-inference --release --features embedded \
  --example smart_harness_eval -- --publish-report report.json publication.json
```

This verifies the original report CID before replacing its two local model
paths with `<MODEL_DIR>/<filename>`. Publication metadata records that redaction
and the original report CID; the copy receives a new CID through the same crate.
Inputs, outputs, timings, scores and exact weights/tokenizer identities remain
unchanged. The saved reports below are these portable copies.

An optional third argument supplies an explicit per-case timeout in milliseconds
for a separate calibration run, for example `report-120s.json 120000`. It also
sets that case's total auxiliary budget to the same value. Keep the original
deadline report alongside it; a longer-budget measurement does not establish
feasibility under the default budget.

An optional fourth argument supplies an instruction file for a separately
identified development comparison. It is recorded in full and labeled as
fixture reuse. `smart_harness_instruction_development.txt` is a shorter candidate
with explicit format examples; a run using it does not measure the bundled
production instruction or a held-out set.

An optional fifth argument supplies a generic system instruction. Use `-` for
the fourth argument to retain the bundled task instruction. This lets a final
development run test system/user role separation while keeping navigation and
classification contracts in their respective user requests. The default system
instruction is empty; the complete override is recorded in the run manifest.

`--baseline-only` instead of the model path evaluates the deterministic side
without loading weights. The baseline is the existing bundled `NudgeClassifier`
and its pending-action gate: pending classes map to narration, other classes
accept an answer. The report preserves the original class and score; the
baseline has no operator-question action. This measures the existing policy,
not a tuned three-class variant. Malformed auxiliary replies and timeouts count
as failures, never as answers. There is no pass-rate gate or superiority claim.

The recorded [30-second run](smart_harness_cpu_30s.json) used the installed
Qwen2.5-0.5B Q4_K_M model on x86-64 CPU with an optimized build. The baseline
matched 3/8 fixture labels. The auxiliary produced six malformed verdicts,
one timeout, and one busy-worker failure after that timeout; strict admission
rejected all eight. Completed malformed responses took 23.2–29.0 seconds.
This run establishes neither useful default classification nor superiority
over the deterministic gate. The complete responses, failures, settings and
asset identities are in the report, including its addressed identity.

The [120-second run](smart_harness_cpu_120s.json) kept that instruction and
removed timeout pressure. All eight outputs still failed strict parsing;
there were no transport errors or timeouts, and responses took 22.4–28.2
seconds. Extending the deadline alone did not make this protocol useful on
the measured model.

The [development run](smart_harness_cpu_development.json) used the shorter
instruction with explicit examples and reused these fixtures. It also produced
eight malformed outputs, taking 21.0–26.1 seconds. The candidate was not promoted
to the bundled instruction.

The final [system-message run](smart_harness_cpu_system.json) kept the bundled
task instruction and added the recorded generic system protocol. Every output
was the unquoted word `answer`, so strict JSON-string admission rejected all
eight. There were no transport errors or timeouts; responses took 27.6–31.4
seconds. This run overlapped workspace verification, so its timings are
observations rather than an isolated performance comparison. The default
system instruction remains empty, and prompt experiments stopped here.

These four runs document a format-following limitation of this model/protocol
combination; useful classification remains unestablished. They do not
demonstrate that the three-class task is impossible or that another model or
protocol would fail.
