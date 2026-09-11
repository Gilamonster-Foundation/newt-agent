# newt-inference

Newt-Agent inference layer: backend trait + local Ollama/vLLM + provider-plugin
host.

- `backend::InferenceBackend` is the trait every backend implements.
- `local::LocalOllamaBackend` and `local::LocalVllmBackend` are built-in HTTP
  backends.
- `embedded::EmbeddedBackend` runs a local quantized Qwen2 model in process
  with the `embedded` feature, enabled by the workspace CLI by default.
- `provider_plugin::ProviderPluginBackend` spawns a subprocess speaking the
  Newt-Provider JSON-RPC protocol — how OpenAI, Anthropic, etc. join via
  opt-in plugin installs.

Also provides the `BackendRegistry` and retry/backoff helpers used by the rest
of the workspace.

Embedded generation admits one worker per process; busy requests fail without
queuing another model load. `complete_with_timeout` bounds the async request,
including loading and generation. Timeout or caller cancellation signals the
worker cooperatively, which retains its slot until it exits. A synchronous load
or forward may still finish after the caller returns. Single-token prefill gives
the pinned Candle Qwen2 engine cancellation checkpoints without an incompatible
multi-token cache mask, trading batch throughput for responsiveness.

`smart_harness::build` creates an auxiliary callback and a credential-free
manifest for both CLI and TUI hosts. The default requires an installed embedded
model and pins its generation device through `EmbeddedBackend::new_cpu`.
That constructor captures immutable weights and tokenizer bytes; the manifest
records their raw CIDs and generation reads those same bytes even if the original
paths change. The immutable model bytes remain resident while the backend lives.
The embedded auxiliary remains CPU-only. An external override requires an
explicit endpoint, model, protocol, and nonempty placement label (`cpu`, `cuda`,
...). Its manifest records `placement_evidence = "operator-declared"`; the client
does not verify the server's hardware. The override may reuse the primary
endpoint and model, with `shares_primary_origin` recording whether their URL
origins match. Calls use the configured timeout and output-token limit, with
transport retries disabled. Separate auxiliary budgets do not guarantee physical
independence or prevent contention on shared servers or hardware. An unavailable
auxiliary returns an error without silently selecting another backend.
Optional `adjudication.system_instruction` precedes the task prompt as a
system-role message. It defaults to empty and applies to both navigation and
classification; task-specific contracts remain in each user prompt. The host
counts both messages against its input-byte budget.

The [classifier comparison](tests/fixtures/README.md) runs the production
auxiliary prompt/parser against curated fixtures and the deterministic baseline,
recording failures and timings without a quality claim.
The external model's 8/8 result reported in
[PR #2263](https://github.com/Gilamonster-Foundation/newt-agent/pull/2263) covers
eight curated fixtures and does not establish general classification quality or
superiority. Smart classification remains experimental.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
