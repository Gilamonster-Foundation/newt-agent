# Upstream Offers to Hermes-Agent

Date: 2026-06-11

This note records Newt-Agent enhancements that look ready to offer upstream to
`NousResearch/hermes-agent`. It is intentionally narrow: these are contributions
that appear useful to Hermes now, are not already fully covered by current
Hermes `main`, and can be proposed as small, reviewable PRs.

The goal is not to upstream Newt architecture wholesale. Hermes is a broad
Python/TypeScript agent with gateway, profile, plugin, and dashboard surfaces;
Newt is a Rust local-first harness. The viable upstream path is to port the
idea and tests to Hermes-native modules.

## Recommended Contributions

### 1. Suspicious empty local-model diagnostics

**Offer:** Add a diagnostics path for local/Ollama-compatible responses that
report generated output tokens but contain no assistant-visible content and no
tool calls.

**Newt reference:** `fix(tui): trace suspicious empty Ollama replies`
(`e35fa5a`). The implementation adds:

- a `--trace` mode that implies debug logging;
- detection for "generated tokens but no visible output";
- one retry with a corrective nudge;
- a targeted final diagnostic naming non-content fields such as
  `reasoning_content` or `thinking`.

**Why Hermes might want it:** Hermes already has empty-response retry and
fallback behavior, but this case is more specific: local backends can burn
tokens into private/non-content fields and return an apparently empty assistant
turn. Naming the structural failure makes user bug reports and provider
compatibility work much easier.

**Suggested Hermes shape:**

- Add a trace-level diagnostic flag or reuse Hermes' existing API debug dump
  controls.
- Detect provider-reported completion/output tokens with empty visible content.
- For local/Ollama-compatible transports, retry once with a short nudge asking
  for assistant-visible text or a valid tool call.
- If still empty, return an actionable diagnostic that includes the non-content
  fields found, without dumping secrets or full payloads by default.

**Likely PR size:** Small. This is the best first upstream offer.

### 2. Empirical local-model capability and tuning cache

**Offer:** Add a local-model capability ledger for Ollama/OpenAI-compatible
endpoints that records tool-call conformance and observed context-window
behavior.

**Newt reference:** `newt-tui/src/probe.rs`, especially `CapabilityEntry`.
Newt tracks:

- tool-call conformance: native tool calls, text-mode JSON, or no tool support;
- declared context window from endpoint metadata;
- empirically safe context;
- overflow points;
- highest successful input size;
- confidence that ratchets up after repeated success and resets on overflow.

**Why Hermes might want it:** Hermes has strong provider catalogs, context
length fallbacks, and Ollama `num_ctx` probing. What Newt adds is a simple
per-local-model behavior record that can answer "does this model actually use
tools correctly here?" and "what input size has actually worked on this
machine?"

**Suggested Hermes shape:**

- Expose this through `hermes doctor`, `hermes models`, or a focused
  `hermes models probe` command.
- Store the cache under the profile-aware Hermes home, not the project tree.
- Keep the cache descriptive first. Do not let it silently override global
  model config until the UX is reviewed.
- Start with Ollama/local OpenAI-compatible backends; avoid the full hosted
  provider matrix in the first PR.

**Likely PR size:** Medium. Valuable after the empty-response diagnostics PR.

### 3. Opt-in project-local config overlay

**Offer:** Let a repository provide a non-secret project-local config overlay
that merges over the user's global Hermes config for that workspace.

**Newt reference:** `newt-core/src/config.rs` project-local `.newt/config.toml`
layering. Newt walks upward from the current directory, finds
`.newt/config.toml`, and deep-merges it over the base config. Arrays are
controlled by an explicit replace/append strategy.

**Why Hermes might want it:** Hermes already supports profiles, workspace
instructions, `terminal.cwd`, cron workdirs, and rich global config. A narrow
project overlay would help repos pin local MCP servers, model preferences,
tool defaults, or workspace-specific non-secret behavior without asking every
developer to hand-edit `~/.hermes/config.yaml`.

**Suggested Hermes shape:**

- Make this opt-in or gated by a clearly named config key at first.
- Exclude secrets and credential-bearing fields entirely; secrets stay in
  `~/.hermes/.env` or provider auth stores.
- Use a conservative allowlist of overlayable sections for the first PR, such
  as MCP server definitions, display/tool behavior, and model selection.
- Include a `hermes config explain` or doctor output showing which project
  overlay was applied.
- Preserve profile semantics: overlays must layer on top of the active profile,
  not bypass it.

**Likely PR size:** Medium to large because of security and migration review.
Good candidate after the two local-model diagnostics contributions.

### 4. First-class trace mode for support bundles

**Offer:** Add a user-facing trace mode that captures structural backend
diagnostics suitable for GitHub issues, while remaining distinct from ordinary
debug logging.

**Newt reference:** `--trace` and `[tui] trace = true` in the same
`e35fa5a` branch. In Newt, trace implies debug and is reserved for backend
compatibility details rather than normal progress logs.

**Why Hermes might want it:** Hermes has many provider adapters and extensive
debug logging, but "turn on trace and attach the sanitized shape" is easier to
support than asking users to find the right log file and redact it manually.

**Suggested Hermes shape:**

- Add a trace mode that writes sanitized request/response shape summaries, not
  raw prompts or secrets.
- Thread the mode through CLI, TUI gateway, and local backend adapters first.
- Include a short issue-template-friendly final message when trace catches a
  known backend incompatibility.
- Keep payload dumps behind an additional explicit flag if needed.

**Likely PR size:** Small if scoped to local backend structural summaries.

## Not Recommended Right Now

- **AGENTS.md / CLAUDE.md loading.** Hermes already has project context loading,
  prompt-injection scanning, and progressive subdirectory discovery.
- **Streamable HTTP MCP transport.** Hermes already supports HTTP MCP surfaces
  and live reload behavior.
- **Context compression, memory, and session recall.** Hermes is currently
  ahead of Newt in these areas; Newt's Hermes-study plan mostly points the
  other direction.
- **Newt's write-file build check hook.** Useful locally, but Hermes already has
  stale-read guards, post-write verification, LSP/syntax delta filtering,
  checkpoints, and stronger write tooling. A build-check hook would need a
  separate approval and safety design.

## Suggested Order

1. Suspicious empty local-model diagnostics.
2. First-class trace mode, if not bundled with item 1.
3. Empirical local-model capability and tuning cache.
4. Opt-in project-local config overlay.

The first two can likely be offered as one PR if the diff stays tight. The
capability cache and project-local overlay should be separate PRs with their
own tests and UX notes.
