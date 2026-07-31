# newt-provider-openai

`newt-provider-openai` is Newt-Agent's opt-in OpenAI inference-provider plugin.
It adapts Newt's provider interface to OpenAI-compatible model APIs without
making that integration part of the default agent binary.

This workspace component is currently unpublished and is not part of Newt's
crates.io release chain.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## Configuration

All configuration is by environment variable:

| Variable | Default | Meaning |
|----------|---------|---------|
| `OPENAI_BASE_URL` | `https://api.openai.com` | Any OpenAI-compatible endpoint (e.g. a llama.cpp router, vLLM, or `https://api.groq.com/openai`) |
| `OPENAI_API_KEY` | — (required) | Bearer token for the endpoint |
| `OPENAI_TIMEOUT_SECS` | `120` | Per-request timeout in whole seconds |
| `OPENAI_MAX_RETRIES` | `2` | Retries for connection/timeout errors and 408/429/5xx responses; `0` disables. Backoff is exponential from 500ms, honoring a numeric `Retry-After` header |

## License

Apache-2.0
