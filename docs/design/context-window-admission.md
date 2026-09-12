# Context-window admission

The declared context window remains an input constraint, with the existing
output reserve. It does not reserve capacity in a server's shared KV pool.

## Session calibration and overflow recovery

The cold estimate uses the configured character ratio (four characters per
token by default). A response's prompt-token usage is compared with the estimate
of that same request, including its advertised tools. The session retains the
largest observed correction and uses it for subsequent requests and compression
targets. Missing usage does not change the prior. An Ollama response suspected
of silent prompt truncation is not a calibration sample.

`Context size has been exceeded` and the supported numbered context errors
classify as `ContextExceeded`. Ordinary transport backoff never resends those
requests. Recovery uses the strongest observed correction, or increases the
applied correction by at least 1.5 when no usage sample exists, then runs the
existing compression pipeline with a smaller target.
There are at most two smaller replacement requests per user turn. Each must
reduce the request estimate; the exact active prompt and newest tool result
remain protected. Failure to produce a smaller fitting projection ends recovery.
The learned correction survives compaction and subsequent turns in the session.

The events file includes `context_exceeded` behavior records with the rejected
request's estimate, attempt number, and corrected projection's token estimate.
`projected_tokens: null` denotes terminal recovery. A successful replacement
still produces the normal completed turn outcome; an unrecovered rejection
produces the explicit `context_exceeded` contract outcome. Consumers of the
closed [benchmark outcome set](../../newt-cli/contract/bench_outcome_values_v1.txt)
must accept the added value before deployment.

Smart mode also records a harness intervention linked to the observed rejection
and its exact recorded request. It does not fabricate a tool result or model
reply. Failed publication prevents another dispatch. Partial server error bytes
remain observations when a subsequent disconnect interrupts the response body.

Regression evidence lives in the
[provider-loop tests](../../newt-core/src/agentic/mod_tests/http_context_exceeded.rs),
[calibration tests](../../newt-core/src/agentic/compress_tests/calibration.rs),
[classification tests](../../newt-core/tests/context_overflow_classification.rs),
and [durable recovery tests](../../newt-core/src/agentic/smart_harness_tests/context_exceeded.rs).
The [projection boundary tests](../../newt-core/src/agentic/context_recovery_tests.rs)
cover protected-history limits and token/byte rounding in both shared and
Responses smart compaction.

Calibration is a conservative heuristic, not exact tokenization. Per-request
admission cannot guarantee that other clients leave room in a shared KV pool.
