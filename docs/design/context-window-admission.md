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
requests. Recovery uses the strongest observed correction, or multiplies the
applied correction by 1.5 when no usage sample exists, up to the existing 3.0
heuristic ceiling. Only overflow guesses are capped: measured ratios above 3.0
remain authoritative. Repeated capacity rejections without token evidence must
not compound the guess until even a later small prompt cannot be dispatched.
Recovery still runs the existing compression pipeline with a smaller target.
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

After the loop accepts an answer or reaches its tool-round limit, an optional
display or summary request can still be rejected. That rejection is recorded
with `projected_tokens: null` and updates session calibration. The accepted
answer or round-cap handoff remains the turn result; the optional request does
not reopen the tool loop. Smart mode does not issue these extra requests.

Regression evidence lives in the
[provider-loop tests](../../newt-core/src/agentic/mod_tests/http_context_exceeded.rs),
[calibration tests](../../newt-core/src/agentic/compress_tests/calibration.rs),
[classification tests](../../newt-core/tests/context_overflow_classification.rs),
[durable recovery tests](../../newt-core/src/agentic/smart_harness_tests/context_exceeded.rs),
and [optional-request adapter tests](../../newt-core/src/agentic/mod_tests/http_context_exceeded_optional.rs).
The [projection boundary tests](../../newt-core/src/agentic/context_recovery_tests.rs)
cover protected-history limits and token/byte rounding in both shared and
Responses smart compaction.

Calibration is a conservative heuristic, not exact tokenization. Per-request
admission cannot guarantee that other clients leave room in a shared KV pool.

## Server token counting

For a configured context bound, each OpenAI-compatible generation request is
probed for a server count after its messages, tools, model, and template options
are assembled. A measured primary request that exceeds the input allowance
enters the bounded context-recovery path before generation is dispatched. The
output allowance remains reserved separately. Optional display and summary
requests retain the terminal-rejection behavior described above.

Measured counts update the conservative session calibration against the
original estimate of that same request, including accepted and refused optional
requests. Admission still probes afresh on each attempt; a prior measurement
does not authorize another request. Smart mode retains the exact counter
response and counting method as request-linked evidence; tokenizer responses
are not model replies.

Exact counting is capability-based:

| Server capability | Measurement | Unsupported behavior |
| --- | --- | --- |
| llama.cpp `/v1/chat/completions/input_tokens` | Submit the complete chat request and use its positive `input_tokens`. | Try the remaining capabilities when the endpoint is absent. |
| vLLM `/v1/chat/completions/render` | Submit the complete chat request and count its rendered `token_ids`. | Nontext inputs retain the calibrated estimate. |
| Older llama.cpp `/apply-template` and `/tokenize` | Require fresh model metadata proving completion-only preprocessing, render the complete chat request, then tokenize that prompt with `add_special: true` and `parse_special: true`. | Missing, ambiguous, multimodal, or unknown capability metadata retains the calibrated estimate. |
| Legacy vLLM chat `/tokenize` without the full renderer | Retain the calibrated estimate. | This endpoint's chat preprocessing is not guaranteed to match generation; its count is not presented as an exact bound. |

Capability discovery and counting repeat for every assembled candidate,
including a candidate rebuilt after context recovery. Counts and capabilities
are not cached across turns. All probes carry the configured bearer credential.
This can require multiple read-only round trips: one for direct llama counting,
two for the vLLM renderer, and six for the guarded older llama path. Endpoint
absence (HTTP 404, 405, or 501) permits fallback; other HTTP failures and malformed
measurements stop dispatch instead of silently weakening enforcement.

These measurements describe the server-rendered input at probe time. They do
not reserve shared KV capacity or make server model/template replacement
between counting and generation atomic. Servers without a supported
exact-counting capability use the session's usage-anchored heuristic and
bounded shrink/recovery behavior. Multimodal and embedding-position accounting
is outside this counter's supported exact subset.

The llama paths share the
[generation renderer and tokenizer](https://github.com/ggml-org/llama.cpp/blob/82d6bb284d1ff1c6ef37f29a4c3b63d1a8b11806/tools/server/server-context.cpp#L4257).
The legacy capability check follows its
[model metadata](https://github.com/ggml-org/llama.cpp/blob/82d6bb284d1ff1c6ef37f29a4c3b63d1a8b11806/tools/server/server-context.cpp#L4566).
vLLM's
[full chat renderer](https://github.com/vllm-project/vllm/blob/46d2b23ac5047a813ebb082122166e4ae09b5f39/vllm/entrypoints/scale_out/render/serving.py#L78)
preserves generation preprocessing; its
[legacy tokenizer](https://github.com/vllm-project/vllm/blob/46d2b23ac5047a813ebb082122166e4ae09b5f39/vllm/entrypoints/serve/tokenize/serving.py)
does not provide that guarantee.

The [counter protocol tests](../../newt-core/src/backend_probe_tests/token_count.rs)
cover capability selection, authenticated probes, malformed counts, and error
classification. The
[provider admission tests](../../newt-core/src/agentic/mod_tests/http_token_count.rs)
cover re-projection, fresh counts before dispatch, protected history, and
calibration from optional requests.

### Durable count records

Smart-harness journal schema remains 2. `RequestIntervention` is an additive
journal variant; existing request, projection, and event encodings are
unchanged. New readers can restore existing schema-2 journals. The existing
refusal of writable schema-1 restoration remains unchanged.

Older readers strictly deserialize the closed `JournalEntry` enum and reject a
journal containing `request_intervention`; upgrade the reader before resuming
such a journal. An unknown-variant decoding error is a reader compatibility
limitation and does not by itself establish that stored bytes were corrupted.
No journal entries are silently skipped or downgraded.

The [smart count tests](../../newt-core/src/agentic/smart_harness_tests/token_count.rs)
cover committed evidence, recovery admission, and persistence failure. The
[reusable request-intervention tests](../../agent-harness/tests/request_intervention.rs)
cover request ownership, fresh restoration, and binding counts to the complete
request template.

## Streaming and request lifetime

The core OpenAI-compatible tool loop and `LocalVllmBackend` request SSE with
usage included. `LocalOllamaBackend` requests Ollama's streamed NDJSON and
requires one JSON object per record and a terminal `done: true` frame. Both
local decoders retain final usage. An observed provider-error record remains
authoritative when a later disconnect or idle timeout interrupts the body.
Separate reasoning deltas stay out of answer text, and the existing inline
reasoning filter remains in place. The shared SSE decoder assembles tool-call
fragments with their observed IDs, then sends names and arguments through the
same validation gate used by complete JSON responses. Empty arguments therefore
mean `{}` in either transport, while malformed or non-object arguments cannot
authorize tool execution. The decoder also requires `[DONE]` and a finish reason.
A server that returns complete JSON instead is decoded once through the existing
response shape. That fallback does not itself trigger a retry; the existing
legacy display reissue still applies. Smart mode records the exact received SSE
bytes before decoding them; the reconstructed message is an interpretation, not
replacement wire evidence. If cancellation drops an unfinished response reader,
its already received byte prefix is committed before the synthetic cancellation
outcome settles that observation.

Core OpenAI-compatible generation uses an idle read timeout configured by
`inference_timeout_secs` in `[tui]` (default 120 seconds); successful reads reset
the bound. Counting probes keep their whole-request bound because they return a
single response. The standalone local backends use `with_timeout(Duration)` for
both the send/header deadline and maximum idle gap between response chunks.
Their owned clients use the same read timeout. An injected client retains its
settings, connection pool, and any stricter deadline. Retry backoff can make the
whole turn longer than one attempt's bounds.

Cancellation and timeout drop the in-flight request. The
[socket regressions](../../newt-inference/tests/local_stream_tests.rs) check that
dropping the future closes an unfinished HTTP/1 connection. A server-side
generation-slot fixture holds its slot until a streamed write observes that
close; the `stream: false` control retains the slot until generation is explicitly
finished. This grounds the disconnect mechanism without claiming that every
remote service releases its scheduler slot or KV allocation on the same schedule.

Legacy mode retains the optional final-display request after accepting an
answer. Its existing idle read deadline allows a progressing stream to continue.
An actual SSE error envelope, including one received before a disconnect or
without `[DONE]`, preserves the accepted answer; a capacity rejection also records
`context_exceeded` and updates calibration. Quoting those words in answer text
is ordinary content. Smart mode continues to omit the extra display request.

The [decoder tests](../../newt-core/src/agentic/openai_sse_strict_tests.rs),
[primary-loop tests](../../newt-core/src/agentic/mod_tests/openai_primary_stream.rs),
and [display-loop tests](../../newt-core/src/agentic/mod_tests/http_display_context_exceeded.rs)
cover these protocol and recovery boundaries without live models.
