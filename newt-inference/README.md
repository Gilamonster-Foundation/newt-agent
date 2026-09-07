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

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
