# newt-inference

Newt-Agent inference layer: backend trait + local Ollama/vLLM + provider-plugin
host.

- `backend::InferenceBackend` is the trait every backend implements.
- `local::LocalOllamaBackend` and `local::LocalVllmBackend` are the only
  backends compiled into the default Newt binary.
- `provider_plugin::ProviderPluginBackend` spawns a subprocess speaking the
  Newt-Provider JSON-RPC protocol — how OpenAI, Anthropic, etc. join via
  opt-in plugin installs.

Also provides the `BackendRegistry` and retry/backoff helpers used by the rest
of the workspace.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
