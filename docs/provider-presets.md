# Provider presets — the hosted-provider roster (Hermes-compatible)

**Disambiguation first:** newt has two provider-shaped surfaces.

| Surface | What it is | Where |
|---|---|---|
| **Provider presets** (this page) | Pure DATA rows behind `newt setup`'s "hosted provider" picker — endpoint, key env vars, model fallbacks. A preset becomes a backend only when you select it. | `~/.newt/providers/*.{toml,yaml,yml}` |
| `[[providers]]` provider *plugins* | Subprocess executables speaking the plugins-protocol JSON-RPC. | `config.toml` `[[providers]]` |

## The roster

Built in: OpenAI, Anthropic, Ollama Cloud, OpenRouter, NVIDIA NIM,
Hugging Face router, Moonshot (Kimi), LM Studio (local, keyless), Venice.ai.
Drop a file in `~/.newt/providers/` (or `<workspace>/.newt/providers/`) to add
a provider or override a builtin by name — later layers win, malformed files
warn and skip, nothing is fatal. `newt providers list` shows the merged result.

## Drop-in formats — newt TOML and Hermes YAML

Field names mirror Hermes Agent's `ProviderProfile` 1:1, so a Hermes provider
definition transposes field-for-field:

```toml
# ~/.newt/providers/acme.toml   (filename stem = the preset name)
display_name = "Acme Inference"
description = "Acme — OpenAI-compatible direct API"
signup_url = "https://acme.example.com/keys"
env_vars = ["ACME_API_KEY"]              # checked in order; first exported wins
base_url = "https://api.acme.example.com/v1"
api_mode = "chat_completions"            # | codex_responses | anthropic_messages | ollama
auth_type = "api_key"
fallback_models = ["acme-large-v3", "acme-small-fast"]
```

The same file works as YAML (`acme.yaml`). **You can also copy a whole Hermes
`~/.hermes/config.yaml` into the directory** — its `custom_providers:` blocks
each become a preset (`allowed_models` → `fallback_models`, a conventional
`<NAME>_API_KEY` env var is synthesized). An inline `api_key` value in a
copied file is **never loaded**: newt stores no plaintext secrets — export the
env var, or paste the key when `newt setup` asks (stored age-encrypted).

`base_url` follows the Hermes convention (usually ending in `/v1`); newt's
transports append `/v1/…` themselves, so the suffix is stripped on mapping.

## Importing Hermes model-provider plugins

Hermes *plugins* are Python (`$HERMES_HOME/plugins/model-providers/<name>/`).
newt converts the declarative ones to preset drop-ins **without executing any
Python** — a whitelist literal-parser reads the `ProviderProfile(...)` fields:

```bash
newt providers import-hermes            # $HERMES_HOME, else ~/.hermes
newt providers import-hermes --dry-run  # report only
```

Hook-bearing plugins (anything that subclasses `ProviderProfile`, defines
functions, or uses non-literal expressions) are **skipped with the reason
printed** — running config-supplied code is the host-RCE class newt refuses on
principle (#1301). Hermes fields newt doesn't consume (`fixed_temperature`,
`default_aux_model`, …) are carried into the emitted TOML as comments, so
nothing is silently lost.

## What is honestly not supported (and why)

- `api_mode = "bedrock_converse"` — newt has no Bedrock Converse transport.
- `auth_type` other than `api_key` (`oauth_device_code`, `oauth_external`,
  `copilot`, `aws_sdk`, `external_process`) — newt has no OAuth/Copilot/AWS
  auth machinery. Such presets still parse and appear in the picker as
  `(unavailable: …reason)` rows — visible, never silently dropped.
- Google Gemini's OpenAI-compat layer — its base path (`/v1beta/openai/`) is
  not `/v1`-shaped, which newt's OpenAI transports require. Gemini models are
  reachable through the `openrouter` preset meanwhile.
- **L1:** `default_headers` are carried for fidelity but not yet sent on the
  wire (threading custom headers through three transports is a declared
  follow-up); the wizard notes this when a preset carries headers.
