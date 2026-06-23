# Newt-Agent — Development Roadmap (Drake-Flight-Sized Steps)

This roadmap breaks v0.x delivery into **small, single-PR steps**. Each step is
sized for one drake flight: one foreman execution, one worker LLM (or small
bake-off), one PR. Steps include their own mocks and tests; nothing in this
roadmap is "implement now, test later."

## Acceptance contract — applies to every step

A step is **only complete** when the resulting PR meets all of:

- [ ] `cargo build --workspace` succeeds.
- [ ] `cargo test --workspace` — all tests pass, including pre-existing ones.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings.
- [ ] `cargo fmt --all -- --check` — clean.
- [ ] **Coverage ≥ 80% workspace-wide** (measured by `cargo llvm-cov`, gate
      enforced in CI from Step 0.3 onward).
- [ ] PR description includes:
  - "What this PR does" (one paragraph)
  - "Test plan" (bulleted; what tests were added, what coverage was added)
  - "Out of scope" (what this PR deliberately doesn't do)
- [ ] No code without tests. No mocks pulled in without an explicit reason.
- [ ] No new dependency added without justification in the PR body.
- [ ] Push hook installed and passing (`just install-hooks` then `just check`).

## Branch naming

`step-NN.M-short-kebab-name` — e.g., `step-03.1-local-ollama-complete`.
The leading number lets `git branch -l 'step-*' | sort` produce the
roadmap order.

## Mock toolbox (introduced in Step 0.4)

| Need | Crate | Why |
|---|---|---|
| HTTP mocking | `wiremock` | Mock Ollama / vLLM / OpenAI-compatible endpoints |
| Filesystem tests | `tempfile` | Real fs, isolated per test |
| Trait mocking | `mockall` | Mock `InferenceBackend` and similar traits |
| CLI tests | `assert_cmd` + `predicates` | Spawn `newt` binary, assert on stdout/exit |
| Async test ergo | `tokio-test` | `assert_pending!` / `assert_ready!` helpers |
| Snapshot tests | `insta` | TUI render snapshots (used from Phase 13+) |
| Subprocess mocking | local helper crate `tests/common` | Stand-in plugin binaries |

---

# Phase 0 — Infrastructure (4 steps, can run mostly in parallel)

These steps add the scaffolding every later step depends on. **Land Phase 0
before kicking off Phase 1+ drake flights.**

## Step 0.1 — Add justfile + cargo-llvm-cov coverage target

**Branch:** `step-00.1-justfile`
**Touches:** `justfile` (new), `Cargo.toml` (dev-deps optional)
**Implements:**
- `just check` — runs fmt + clippy + test
- `just build`, `just release`, `just test`, `just fmt`, `just lint`
- `just cov` — runs `cargo llvm-cov --workspace --html` and prints summary
- `just cov-ci` — runs `cargo llvm-cov --workspace --lcov --output-path lcov.info --fail-under-lines 80`
- `just install-hooks` — symlinks `.githooks` as `core.hooksPath`
**Tests:** N/A (justfile is config); manually confirm `just check` passes.
**Mocks:** none.
**Out of scope:** CI workflow file (that's Step 0.3).
**Estimated diff:** ~80 lines.

## Step 0.2 — Add .githooks/pre-push + CLAUDE.md

**Branch:** `step-00.2-pre-push-hook`
**Touches:** `.githooks/pre-push` (new, executable), `CLAUDE.md` (new), `AGENTS.md` (new)
**Implements:**
- `.githooks/pre-push` — runs `just check` + `just cov-ci`; mirrors CI
  exactly (comment: `# PIPELINE PARITY: mirrors .github/workflows/ci.yml`).
- `CLAUDE.md` — repo-specific agent instructions (build commands, hook
  install, vi-not-emacs reminder, link to ROADMAP.md).
- `AGENTS.md` — same content for non-Claude agents (or symlink).
**Tests:** Bash-level: `bash .githooks/pre-push` runs successfully.
**Mocks:** none.
**Out of scope:** the CI workflow (Step 0.3).
**Estimated diff:** ~100 lines.

## Step 0.3 — Add .github/workflows/ci.yml with coverage gate

**Branch:** `step-00.3-ci-workflow`
**Touches:** `.github/workflows/ci.yml` (new)
**Implements:**
- Jobs: `lint` (fmt + clippy), `test` (cargo test --workspace),
  `coverage` (cargo llvm-cov, fail if < 80%).
- Runs on PRs to `main` and pushes to `main`.
- Header comment: `# HOOK PARITY: mirrored by .githooks/pre-push`.
- Caches `~/.cargo` and `target/` across runs.
**Tests:** PR itself proves the workflow runs (will be the first run).
**Mocks:** none.
**Out of scope:** release/wheel-build workflow (Phase 11).
**Estimated diff:** ~80 lines.

## Step 0.4 — Add tests/common/ helper crate + dev-dependency wiring

**Branch:** `step-00.4-test-helpers`
**Touches:** `tests/common/Cargo.toml` (new), `tests/common/src/lib.rs` (new),
              root `Cargo.toml` (add to workspace), per-crate `Cargo.toml`
              (`[dev-dependencies] tests-common.workspace = true` where used).
**Implements:**
- `tests-common` crate exposes:
  - `init_tracing()` — installs a test subscriber.
  - `MockBackend` — `InferenceBackend` impl with configurable replies.
  - `tempdir()` — wraps `tempfile::tempdir()`.
  - `mock_plugin_binary(path, replies)` — writes a tiny mock plugin script.
- Add dev-deps to workspace pins: `wiremock = "0.6"`, `tempfile = "3"`,
  `mockall = "0.13"`, `assert_cmd = "2"`, `predicates = "3"`, `tokio-test`,
  `insta = "1"`.
**Tests:** smoke-test the test helpers themselves (round-trip MockBackend).
**Mocks:** N/A (this step IS the mock toolbox).
**Out of scope:** using any of these helpers in real tests yet.
**Estimated diff:** ~150 lines.

---

# Phase 1 — newt-core expansions (3 steps)

## Step 1.1 — Add Config type + TOML loader

**Branch:** `step-01.1-config-toml`
**Touches:** `newt-core/src/config.rs` (new), `newt-core/src/lib.rs`,
              `newt-core/Cargo.toml` (+ `toml`).
**Implements:**
- `Config { backends: Vec<BackendConfig>, providers: Vec<ProviderConfig>, default_tier_order: Vec<Tier> }`.
- `Config::load(path: &Path) -> Result<Self>`.
- `Config::resolve()` — searches `./newt.toml`, `~/.newt/config.toml`,
  `/etc/newt/config.toml`; returns the first hit, or `Config::default()`.
**Tests:** 5+ — defaults, load happy path, missing file → default,
malformed TOML → error, env-var override of search path.
**Mocks:** `tempfile` for fs.
**Estimated diff:** ~180 lines.

## Step 1.2 — Add SessionId newtype (UUID wrapper)

**Branch:** `step-01.2-session-id`
**Touches:** `newt-core/src/session.rs` (new), `newt-core/src/lib.rs`,
              `newt-core/Cargo.toml` (+ `uuid`).
**Implements:** `SessionId(Uuid)` with `new()`, `Display`, `FromStr`, serde
impls.
**Tests:** 4+ — generate, parse roundtrip, display matches input, invalid
parse rejected.
**Mocks:** none.
**Estimated diff:** ~70 lines.

## Step 1.3 — Expand Router with confidence + tier overrides

**Branch:** `step-01.3-router-confidence`
**Touches:** `newt-core/src/router.rs`.
**Implements:**
- `Router::classify_detailed(prompt) -> Classification { tier, confidence, reasons }`.
- `Router::with_override(tier)` — force-tier escape hatch.
- Keep `classify()` as a thin wrapper.
**Tests:** existing 3 stay green + 6 new — confidence bounds, override always
returns its tier, reasons populated.
**Mocks:** none.
**Estimated diff:** ~120 lines.

---

# Phase 2 — newt-inference foundations (4 steps)

## Step 2.1 — Add Message builders + ChatRequest helpers

**Branch:** `step-02.1-message-builders`
**Touches:** `newt-inference/src/backend.rs`.
**Implements:**
- `Message::user("...")`, `Message::system("...")`, `Message::assistant("...")`.
- `ChatRequest::new()` + builder methods `system()`, `user()`, `assistant()`,
  `max_tokens()`.
**Tests:** 5+ — each builder, chaining, max_tokens propagation, serde
roundtrip of ChatRequest.
**Mocks:** none.
**Estimated diff:** ~120 lines.

## Step 2.2 — Add BackendRegistry

**Branch:** `step-02.2-backend-registry`
**Touches:** `newt-inference/src/registry.rs` (new), `newt-inference/src/lib.rs`.
**Implements:**
- `BackendRegistry { entries: Vec<Arc<dyn InferenceBackend>> }`.
- `register(backend)`, `pick(tier)` — returns first that `supports_tier(t)`.
- `names()`, `len()`, `is_empty()`.
**Tests:** 6+ — registration order preserved, pick returns first match, pick
returns `NoBackendForTier` when none, mixed tier support, names list correct.
**Mocks:** `MockBackend` from `tests-common`.
**Estimated diff:** ~150 lines.

## Step 2.3 — Add streaming reply types

**Branch:** `step-02.3-stream-types`
**Touches:** `newt-inference/src/stream.rs` (new), `newt-inference/src/lib.rs`.
**Implements:**
- `ChatChunk { delta: String, model_id: String, is_final: bool }`.
- `ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk>> + Send>>`.
- `InferenceBackend::stream()` default method returning
  `not_supported` error (backends override later).
**Tests:** 4+ — chunk roundtrip, default `stream()` returns NotSupported,
collecting a stream of chunks into a `ChatReply` helper works.
**Mocks:** synthetic in-memory stream via `futures::stream::iter`.
**Estimated diff:** ~130 lines.

## Step 2.4 — Add ModelId & audit-trail wiring

**Branch:** `step-02.4-model-id-audit`
**Touches:** `newt-inference/src/backend.rs`, `newt-core/src/lib.rs`.
**Implements:**
- `ModelId(String)` newtype (newt-core).
- Replace string `model_id: String` in `ChatReply` / `BackendInfo` with
  `ModelId`.
- `ChatReply::audit_string()` — formatted "backend=X model_id=Y" line.
**Tests:** 5+ — newtype roundtrip, audit_string format, serde compat.
**Mocks:** none.
**Estimated diff:** ~100 lines.

---

# Phase 3 — LocalOllamaBackend (3 steps)

## Step 3.1 — Implement LocalOllamaBackend.complete()

**Branch:** `step-03.1-local-ollama-complete`
**Touches:** `newt-inference/src/local.rs`, `newt-inference/Cargo.toml`.
**Implements:**
- POST to `<endpoint>/api/chat` with Ollama JSON shape
  `{ model, messages, stream: false }`.
- Parse response; populate `ChatReply { content, model_id }`.
- Map HTTP errors to `NewtError::Backend`.
**Tests:** 6+ using `wiremock` — happy path, non-200, malformed JSON, empty
content, timeout, model_id correctly returned.
**Mocks:** `wiremock::MockServer` for HTTP.
**Estimated diff:** ~180 lines.

## Step 3.2 — Endpoint discovery fallback chain

**Branch:** `step-03.2-ollama-endpoint-discovery`
**Touches:** `newt-inference/src/local.rs`.
**Implements:**
- `LocalOllamaBackend::discover()` — try in order:
  in-cluster proxy → `REDACTED-HOST` → `REDACTED-HOST` →
  `REDACTED-HOST` → `http://127.0.0.1:11434`. First reachable wins.
- Reachability = HTTP GET `/api/tags` succeeds within 500ms.
**Tests:** 5+ — first wins, fallthrough on 5xx, all-down returns
NoEndpoint error, override via env var, custom list via config.
**Mocks:** multiple `wiremock` servers on random ports.
**Estimated diff:** ~150 lines.

## Step 3.3 — Add timeout + retry-with-backoff

**Branch:** `step-03.3-ollama-retry`
**Touches:** `newt-inference/src/local.rs`, `newt-inference/Cargo.toml`
              (+ `tokio-retry` or hand-rolled).
**Implements:** retry up to 3× on connect-reset / 5xx with 250ms / 500ms /
1000ms backoff. Total timeout cap = 60s. Surface attempts in `tracing`.
**Tests:** 4+ — retries on 503, gives up after N, total cap honored,
non-retryable errors (4xx) don't retry.
**Mocks:** `wiremock` with sequenced response chain.
**Estimated diff:** ~120 lines.

---

# Phase 4 — LocalVllmBackend (2 steps)

## Step 4.1 — Implement LocalVllmBackend.complete()

**Branch:** `step-04.1-local-vllm-complete`
**Touches:** `newt-inference/src/local.rs`.
**Implements:**
- POST to `<endpoint>/v1/chat/completions` with OpenAI-compatible JSON
  `{ model, messages, max_tokens }`.
- Parse `choices[0].message.content` + `model`.
**Tests:** 6+ using `wiremock` — mirror Step 3.1 shape but vLLM-flavored.
**Mocks:** `wiremock`.
**Estimated diff:** ~180 lines.

## Step 4.2 — vLLM list_models + endpoint health

**Branch:** `step-04.2-vllm-list-models`
**Touches:** `newt-inference/src/local.rs`.
**Implements:** `GET /v1/models` → `Vec<ModelInfo>`; used by `newt doctor`.
**Tests:** 4+ — happy path, empty list, server down, malformed JSON.
**Mocks:** `wiremock`.
**Estimated diff:** ~100 lines.

---

# Phase 5 — plugins-protocol library (3 steps)

## Step 5.1 — JSON-RPC frame reader/writer

**Branch:** `step-05.1-jsonrpc-framing`
**Touches:** `plugins-protocol/src/transport.rs` (new),
              `plugins-protocol/src/lib.rs`.
**Implements:**
- Newline-delimited JSON-RPC 2.0 framing over an `AsyncRead` / `AsyncWrite`.
- `read_message()` / `write_message()` helpers.
- Error types for malformed frames.
**Tests:** 6+ — single message, batched, partial reads, malformed JSON,
EOF mid-message, request/response/notification shapes.
**Mocks:** `tokio::io::duplex()` for paired in-memory streams.
**Estimated diff:** ~200 lines.

## Step 5.2 — PluginClient (host-side spawn + RPC)

**Branch:** `step-05.2-plugin-client`
**Touches:** `plugins-protocol/src/client.rs` (new).
**Implements:**
- `PluginClient::spawn(command, env_pass) -> Self`.
- `initialize()`, `list_models()`, `complete(req)`, `shutdown()`.
- Drops the subprocess on `Drop`.
**Tests:** 5+ using a mock plugin binary (small shell script written by
`tests-common::mock_plugin_binary`) — handshake, complete, shutdown, bad
exit code, hanging plugin (timeout).
**Mocks:** real subprocess of a tiny shell script.
**Estimated diff:** ~220 lines.

## Step 5.3 — PluginServer reference SDK (Rust)

**Branch:** `step-05.3-plugin-server-sdk`
**Touches:** `plugins-protocol/src/server.rs` (new),
              `plugins-protocol/examples/echo_plugin.rs` (new).
**Implements:**
- `PluginServer::new(handlers) -> Self`; `run_stdio()` event loop.
- `examples/echo_plugin.rs` — a runnable echo plugin used in tests.
**Tests:** 5+ — round-trip via the echo example using PluginClient.
**Mocks:** uses the echo example as the "real" subprocess.
**Estimated diff:** ~200 lines.

---

# Phase 6 — ProviderPluginBackend (2 steps)

## Step 6.1 — ProviderPluginBackend.complete() over PluginClient

**Branch:** `step-06.1-provider-plugin-backend`
**Touches:** `newt-inference/src/provider_plugin.rs`.
**Implements:** forward `complete()` calls to a `PluginClient`. Plugin
binary path comes from constructor.
**Tests:** 5+ via the echo plugin from Step 5.3 — happy path, plugin
crashed, plugin returned error, unsupported model, model_id propagated.
**Mocks:** echo plugin subprocess.
**Estimated diff:** ~150 lines.

## Step 6.2 — Provider discovery from Config

**Branch:** `step-06.2-provider-discovery`
**Touches:** `newt-inference/src/provider_plugin.rs`, `newt-core/src/config.rs`.
**Implements:** `BackendRegistry::load_from_config(cfg)` — iterates
`cfg.providers`, spawns a `ProviderPluginBackend` for each, registers in
order.
**Tests:** 4+ — empty providers list, single provider, multiple providers,
missing binary surfaces a loud config error (not silent skip).
**Mocks:** mock plugin binaries.
**Estimated diff:** ~140 lines.

---

# Phase 7 — newt-tools (4 steps)

## Step 7.1 — Implement read()

**Branch:** `step-07.1-tools-read`
**Touches:** `newt-tools/src/lib.rs`, split into `read.rs`.
**Implements:** path validation (no `..` escape outside root), UTF-8 read,
size cap (e.g., 5 MiB → ToolError::TooLarge).
**Tests:** 6+ — happy path, missing file, non-UTF-8 bytes, too-large file,
path-escape rejected, empty file.
**Mocks:** `tempfile`.
**Estimated diff:** ~150 lines.

## Step 7.2 — Implement search()

**Branch:** `step-07.2-tools-search`
**Touches:** `newt-tools/src/search.rs` (new), `newt-tools/Cargo.toml`
              (+ `ignore`, `grep-searcher`, `grep-regex`).
**Implements:** ripgrep-style search; respects `.gitignore`; returns
`Vec<Hit { path, line_number, line }>`; result cap (1000 hits).
**Tests:** 6+ — single hit, multiple hits, no hits, .gitignore respected,
binary files skipped, regex syntax errors surfaced.
**Mocks:** `tempfile` populated with a synthetic tree.
**Estimated diff:** ~220 lines.

## Step 7.3 — Implement apply_patch()

**Branch:** `step-07.3-tools-apply-patch`
**Touches:** `newt-tools/src/patch.rs` (new), `newt-tools/Cargo.toml`
              (+ `diffy` or `imara-diff`).
**Implements:** parse unified diff, apply to in-tree files, atomic
write-and-rename per file, surface conflict on context mismatch.
**Tests:** 7+ — single-file patch, multi-file patch, context mismatch
rejected, malformed diff rejected, file-not-found surfaced, atomic
rename verified, partial-apply rolled back on first failure.
**Mocks:** `tempfile`.
**Estimated diff:** ~280 lines.

## Step 7.4 — Implement edit() as single-file patch convenience

**Branch:** `step-07.4-tools-edit`
**Touches:** `newt-tools/src/lib.rs`.
**Implements:** thin wrapper over `apply_patch()` for single-file diffs.
**Tests:** 4+ — round-trip, conflict surfaced, missing path, empty patch.
**Mocks:** `tempfile`.
**Estimated diff:** ~80 lines.

---

# Phase 8 — newt-mcp-server (5 steps)

## Step 8.1 — Stdio JSON-RPC server scaffolding

**Branch:** `step-08.1-mcp-stdio-server`
**Touches:** `newt-mcp-server/src/server.rs` (new), `newt-mcp-server/src/lib.rs`.
**Implements:** reuse `plugins-protocol::transport` (or re-export) for
framing; main event loop reads requests, dispatches to handler trait,
writes replies.
**Tests:** 5+ via `tokio::io::duplex` paired streams — round-trip request,
unknown method → JSON-RPC error, malformed input handled, multiple
requests serially, shutdown on EOF.
**Mocks:** in-memory duplex streams.
**Estimated diff:** ~200 lines.

## Step 8.2 — MCP initialize + tools/list

**Branch:** `step-08.2-mcp-initialize-list`
**Touches:** `newt-mcp-server/src/handlers.rs` (new).
**Implements:** MCP `initialize` handshake, `tools/list` returning the
five tool schemas (with empty `tools/call` placeholder).
**Tests:** 4+ — initialize handshake matches MCP spec version, tools/list
returns five names, schema valid JSON-schema, unknown method errors out.
**Mocks:** in-memory streams.
**Estimated diff:** ~180 lines.

## Step 8.3 — MCP code.read tool

**Branch:** `step-08.3-mcp-code-read`
**Touches:** `newt-mcp-server/src/handlers.rs`.
**Implements:** `tools/call name=code.read` → `newt_tools::read()`; wrap
errors as MCP tool errors.
**Tests:** 5+ — happy path, missing file, non-UTF-8, path-escape, args
schema mismatch surfaces validation error.
**Mocks:** `tempfile` + in-memory streams.
**Estimated diff:** ~120 lines.

## Step 8.4 — MCP code.edit + code.search tools

**Branch:** `step-08.4-mcp-code-edit-search`
**Touches:** `newt-mcp-server/src/handlers.rs`.
**Implements:** `tools/call` for `code.edit` and `code.search` wired to
`newt-tools`.
**Tests:** 6+ — edit happy + conflict, search happy + empty + regex error.
**Mocks:** `tempfile` + in-memory streams.
**Estimated diff:** ~180 lines.

## Step 8.5 — MCP goal.run tool (router + inference)

**Branch:** `step-08.5-mcp-goal-run`
**Touches:** `newt-mcp-server/src/handlers.rs`, `newt-mcp-server/Cargo.toml`.
**Implements:** `tools/call name=goal.run` → classify via Router → pick
backend via Registry → call `.complete()` → return `{ content, model_id }`.
**Tests:** 5+ — happy path with MockBackend, no backend for tier,
override tier respected, propagates errors, model_id reported.
**Mocks:** `MockBackend`.
**Estimated diff:** ~180 lines.

---

# Phase 9 — newt-acp-worker (5 steps)

## Step 9.1 — agent-client-protocol dep + initialize/new_session

**Branch:** `step-09.1-acp-init-newsession`
**Touches:** `newt-acp-worker/Cargo.toml` (+ `agent-client-protocol`),
              `newt-acp-worker/src/lib.rs`, `newt-acp-worker/src/server.rs` (new).
**Implements:** handshake + new_session storing `workspace_path`.
**Tests:** 4+ — handshake echoes capabilities, new_session stores path,
unknown session id surfaces error, second initialize is idempotent.
**Mocks:** in-memory transport via the ACP crate's test helpers (or
duplex streams).
**Estimated diff:** ~200 lines.

## Step 9.2 — set_session_model + per-task model override

**Branch:** `step-09.2-acp-set-session-model`
**Touches:** `newt-acp-worker/src/server.rs`.
**Implements:** map `goal.model` → backend selection, falling through to
Router default when unset.
**Tests:** 4+ — model override picks named backend, missing model
surfaces error, unset model uses Router, model_id reflected in reply.
**Mocks:** `MockBackend` registry.
**Estimated diff:** ~130 lines.

## Step 9.3 — prompt handler → inference

**Branch:** `step-09.3-acp-prompt-inference`
**Touches:** `newt-acp-worker/src/server.rs`.
**Implements:** route `acp.prompt` through Router + Registry; stream
events back to ACP client.
**Tests:** 5+ — happy path streams chunks, stop_reason=end_turn,
stop_reason=max_turns, backend error surfaces ACP error, content roundtrip.
**Mocks:** streaming `MockBackend` from Phase 2.
**Estimated diff:** ~200 lines.

## Step 9.4 — Diff capture + empty-diff crash

**Branch:** `step-09.4-acp-diff-capture`
**Touches:** `newt-acp-worker/src/diff.rs` (new), `newt-acp-worker/Cargo.toml`
              (+ `git2`).
**Implements:** post-prompt `git diff --no-color` of workspace_path;
if empty → exit non-zero with `EmptyDiff` error per
`feedback_empty_diff_is_a_crash`.
**Tests:** 5+ — non-empty diff returned, empty diff → error, untracked
files counted, repo-not-found surfaces error, binary diffs handled.
**Mocks:** `tempfile` initialized as a git repo (`git2`).
**Estimated diff:** ~200 lines.

## Step 9.5 — TaskReply with mandatory model_id

**Branch:** `step-09.5-acp-task-reply`
**Touches:** `newt-acp-worker/src/server.rs`.
**Implements:** emit `TaskReply { content, model_id, diff }`; reject any
code path that would omit `model_id`.
**Tests:** 4+ — model_id present on success, model_id present on failure,
serialization stable, missing model_id = compile error (use type-state
or `#[must_use]`).
**Mocks:** `MockBackend`.
**Estimated diff:** ~120 lines.

## Step 9.6 — newt-eval: agentic runner support + deferred cases

**Branch:** `step-9.6-eval-agentic-runner`
**Touches:** `newt-eval/src/runner.rs`, `newt-eval/cases-deferred/015-agentic-create-file/`,
              `newt-eval/cases-deferred/016-agentic-refactor-fn/`.
**Implements:** Add `agentic_mode: bool` to `RunnerConfig` (+ builder). When set,
`drive_acp` dispatches `agentic_prompt` instead of `prompt`. Write two eval cases
in `cases-deferred/` — these define the acceptance criteria for Step 9.8 before
that implementation exists. Cases stay deferred until the worker exposes the
method (Step 9.8); use the `cases-deferred/` holding pattern already established
by `006-cross-host-rename`.
**Cases:**
- `015-agentic-create-file` — prompt asks the worker to create a new Rust source
  file; evaluators: `diff_nonempty`, `diff_applies`.
- `016-agentic-refactor-fn` — prompt asks for a multi-step extract-function
  refactor; evaluators: `diff_nonempty`, `rust_compiles`.
**Tests:** Unit test `runner_config_agentic_mode_builder` alongside the existing
`config_builders_compose`; no e2e test because the worker doesn't expose
`agentic_prompt` yet — that's intentional.
**Out of scope:** mock responses for the deferred cases (added in Step 9.9).
**Mocks:** none (runner unit tests only).
**Estimated diff:** ~120 lines.

## Step 9.7 — Extract agentic loop to `newt-core::agentic`

**Branch:** `step-9.7-newt-core-agentic`
**Touches:** `newt-core/src/agentic/` (new module), `newt-tui/src/lib.rs`.
**Implements:** Move `ChatCtx`, `chat_complete`, `openai_chat_complete`,
`execute_tool`, and all tool-definition helpers from `newt-tui/src/lib.rs` into
`newt-core/src/agentic/`. Keep `ChatCtx` as the concrete type — do **not**
introduce an `InferenceBackend` trait at this step (YAGNI: there is currently
one implementor). Pub-use the new types from `newt_core` top-level for clean
import paths. `newt-tui` becomes a thin wrapper that constructs a `ChatCtx`
and calls `newt_core::agentic::chat_complete()`.

The wiremock agentic-loop tests travel with the code into
`newt-core/src/agentic/` — this is the majority of the diff. As of #201
they live in two blocks of `newt-tui/src/lib.rs`: the `openai_chat_complete`
suite (~line 5486) and the larger `mod http_loop_tests` (~line 7364, covering
streaming, overflow trim-and-retry, mid-loop trim, empty-summary fallbacks,
and the read-only nudge). Both move; grep for `MockServer::start` to find
them all rather than trusting a line number. No behavioral change; CI green
before and after.
**Tests:** All existing wiremock-based agentic loop tests migrate to
`newt-core/src/agentic/` (Ollama path, OpenAI path, overflow retry,
read-only nudge, cap-exit fallback). Net test count should be unchanged.
Watch the coverage gate: this code is well-tested, so moving it from
`newt-tui` to `newt-core` shifts where the covered lines count but keeps the
workspace total flat — the 80% floor must still clear.
**Out of scope:** `InferenceBackend` trait abstraction (future step when a
second concrete backend exists).
**Mocks:** existing `wiremock` HTTP mocks travel with the tests unchanged.
**Estimated diff:** ~400 lines moved, ~50 lines new glue.

## Step 9.8 — `agentic_prompt` ACP method + per-session config

**Branch:** `step-9.8-acp-agentic-prompt`
**Touches:** `newt-acp-worker/src/server.rs`, `newt-core/src/agentic/config.rs` (new).
**Implements:**
- `AgenticConfig` struct in `newt-core::agentic` — tunables extracted from
  `ChatCtx` (`max_tool_rounds`, `tool_output_lines`, `mid_loop_trim_threshold`,
  `trim_ratio`, `build_check_cmd`, `num_ctx`, `cap_exit_prompt`,
  `read_only_nudge_prompt`). `impl Default` mirrors existing `ChatCtx` defaults.
- Extend `Session` with `agentic_config: AgenticConfig`.
- Parse optional `"agentic"` block in `new_session` params; per-call overrides
  also accepted in `agentic_prompt` params (same fields, finer-grained).
- Wire `"agentic_prompt"` into the dispatch table; handler calls
  `newt_core::agentic::run_agentic_loop()`, returns `TaskReply` with
  `emission_shape: "agentic_loop"`, plus `tool_rounds` and `exited_via_cap`
  fields in the reply.
- Update capabilities advertisement to include `"agentic_prompt"`.
**Tests:** 5+ ACP handler unit tests with wiremock — happy path produces
non-empty diff, `tool_rounds` is non-zero, unknown session id returns error,
per-call `max_tool_rounds` override is respected, `emission_shape` is
`"agentic_loop"` in the wire reply.
**Out of scope:** streaming progress events (requires ACP protocol extension).
**Mocks:** `wiremock` Ollama stand-in.
**Estimated diff:** ~250 lines.

## Step 9.9 — Promote deferred agentic eval cases to CI

**Branch:** `step-9.9-eval-agentic-cases-promote`
**Touches:** `newt-eval/cases/015-agentic-create-file/`,
              `newt-eval/cases/016-agentic-refactor-fn/`,
              `newt-eval/tests/mock_e2e.rs`.
**Implements:** Add `mock_response` blocks (canned diffs) to the two cases
from Step 9.6. Move them from `cases-deferred/` to `cases/`. Add both to the
`mock_e2e` integration test so they run under `just check`. These become the
ratchet-locked regression gate for `agentic_prompt` behavior.
**Tests:** 2 new mock e2e test cases (one per promoted case). Each drives the
worker with `agentic_mode: true`, verifies `diff_nonempty` and at least one
domain evaluator. Coverage ratchet must not drop.
**Out of scope:** live-mode eval tuning (do that in a follow-on when testing
against a real model).
**Mocks:** wiremock returning the canned diffs from the case fixtures.
**Estimated diff:** ~100 lines.

### Steps 9.6–9.9 are the *exposure* track — related work tracked separately

These four steps make the existing, battle-tested TUI loop reachable from
foreman/ACP and pin its behavior with eval cases. Two adjacent tracks are
deliberately **not** folded in here, to keep each step reviewable:

- **Loop hardening** — once Step 9.7 lands `newt-core::agentic`, that module
  (not `newt-tui`) is the home for robustness work that should benefit both
  the TUI and ACP paths: salvaging `TextMode` tool calls (#214 — models that
  emit tool-call JSON in `content` are detected via `ToolConformance` but
  never dispatched), the fresh-install permissions gap (#215 — a config with
  no `[tui]` block resolves to read-only caveats, which forces advisory drift:
  the agent narrates instead of editing), and surfacing the effective
  `num_ctx` under `NEWT_DEBUG` (#216). All sequenced **after** Step 9.7 so
  they land in the shared module.
- **Model selection** — which local models drive the loop (#217 — pin
  agentic-model tiers in `DgxConfig`: 1-Spark `qwen3-coder:30b`, 2-Spark
  `Qwen3-235B-A22B` / `Qwen3-Coder-480B`) and the DGX/Spark serving tier for
  larger agentic coders (#218 — 2-Spark vLLM tensor-parallel; prereq for the
  Tier-B formations). The eval cases here (9.6/9.9) are model-agnostic by
  design so they gate *loop behavior*, not a specific model. (Supersedes the
  closed #46 model×size matrix.)

---

# Phase 10 — newt-cli polish (3 steps)

## Step 10.1 — newt doctor

**Branch:** `step-10.1-cli-doctor`
**Touches:** `newt-cli/src/doctor.rs` (new), `newt-cli/src/lib.rs`.
**Implements:** for each configured backend: probe (`/api/tags` for
Ollama, `/v1/models` for vLLM, plugin `initialize` for providers); print
table of status + model_id.
**Tests:** 5+ using `assert_cmd` + `wiremock` — all healthy, one down,
plugin missing, no backends, config not found.
**Mocks:** `wiremock`, mock plugin binaries.
**Estimated diff:** ~180 lines.

## Step 10.2 — newt config

**Branch:** `step-10.2-cli-config`
**Touches:** `newt-cli/src/config_cmd.rs` (new).
**Implements:** load Config + print as resolved TOML (with sources noted
in comments).
**Tests:** 4+ — default config, custom file, env var override, malformed
file surfaces error.
**Mocks:** `tempfile`, `assert_cmd`.
**Estimated diff:** ~120 lines.

## Step 10.3 — `--config <path>` global flag

**Branch:** `step-10.3-cli-config-flag`
**Touches:** `newt-cli/src/lib.rs`.
**Implements:** add `-c / --config` global flag; threads through to every
subcommand that needs Config.
**Tests:** 3+ — flag respected, env var fallback, default search order.
**Mocks:** `assert_cmd`.
**Estimated diff:** ~80 lines.

---

# Phase 11 — Distribution (3 steps)

## Step 11.1 — pyproject.toml for newt-agent wheel (maturin)

**Branch:** `step-11.1-pyproject-cli`
**Touches:** `pyproject.toml` (new), `python/newt_agent/__init__.py` (new).
**Implements:** maturin `bindings = "bin"` config publishing `newt` on
PATH; thin `python -m newt_agent` re-entry.
**Tests:** local `maturin develop --release` succeeds and `newt --help`
works inside the venv. Add a `pytest` smoke under `tests/python/`.
**Mocks:** none.
**Estimated diff:** ~80 lines.

## Step 11.2 — pyproject.toml for newt-mcp-server wheel

**Branch:** `step-11.2-pyproject-mcp`
**Touches:** `newt-mcp-server/pyproject.toml` (new),
              `newt-mcp-server/python/newt_mcp_server/__init__.py` (new).
**Implements:** identical pattern, separate wheel so IDEs can install just
the MCP server.
**Tests:** smoke-test wheel install + `newt-mcp-server --help`.
**Mocks:** none.
**Estimated diff:** ~80 lines.

## Step 11.3 — Release workflow (wheels on tag)

**Branch:** `step-11.3-release-workflow`
**Touches:** `.github/workflows/release.yml` (new).
**Implements:** triggered by `v*` tag; builds wheels for linux+mac+windows
via `maturin build`; publishes to PyPI; uploads sdist + binaries as
release assets.
**Tests:** workflow itself acts as the test on first tag push.
**Mocks:** none.
**Estimated diff:** ~120 lines.

---

# Phase 12 — Reference provider plugin (3 steps, separate sub-tree)

These live under `providers/openai/` in the same workspace but are **never**
default-built and are published as separate crates/wheels.

## Step 12.1 — providers/openai crate skeleton

**Branch:** `step-12.1-provider-openai-skeleton`
**Touches:** `providers/openai/Cargo.toml` (new),
              `providers/openai/src/main.rs` (new),
              root `Cargo.toml` (add `providers/openai` to members).
**Implements:** binary `newt-provider-openai` linking `plugins-protocol::PluginServer`; stubs that bail.
**Tests:** smoke test that the binary starts and answers `initialize`.
**Mocks:** PluginClient drives the real binary.
**Estimated diff:** ~120 lines.

## Step 12.2 — OpenAI HTTP client

**Branch:** `step-12.2-provider-openai-http`
**Touches:** `providers/openai/src/client.rs` (new).
**Implements:** POST `/v1/chat/completions` to `OPENAI_API_KEY`-authed
endpoint; map errors.
**Tests:** 6+ with `wiremock` against the OpenAI API shape.
**Mocks:** `wiremock`.
**Estimated diff:** ~200 lines.

## Step 12.3 — providers/openai pyproject + PyPI wiring

**Branch:** `step-12.3-provider-openai-pyproject`
**Touches:** `providers/openai/pyproject.toml` (new).
**Implements:** `pip install newt-provider-openai` installs the binary on
PATH. Release workflow extended to publish this wheel.
**Tests:** maturin develop smoke, plus an end-to-end test that Newt
discovers the plugin from a test config and round-trips a chat through
a wiremocked OpenAI endpoint.
**Mocks:** `wiremock` + tempfile config.
**Estimated diff:** ~100 lines.

---

# Phase 13 — TUI (deferred to a follow-up roadmap)

TUI work is hard to unit-test to 80% coverage with mocks alone — it needs
snapshot testing (`insta`) and a terminal simulator. We'll plan that phase
once Phases 0–12 are landed and `newt worker` is dogfooded end-to-end.

---

# Phase 14 - `newt dgx` command suite (9 steps)

Native NVIDIA DGX / Ollama endpoint management, ported from the hermes-agent
`hermes dgx` plugin (NousResearch/hermes-agent#28009) and retargeted to newt's
Rust surface. Unlike hermes (a Python plugin loaded via `plugin.yaml`), this is
a built-in `newt dgx` subcommand group plus a DGX-aware backend - newt is
opinionated and single-binary, not plugin-extensible. Reuses the already-landed
`Config`, `LocalOllamaBackend` / `LocalVllmBackend`, `Router`, and
`BackendRegistry`. No new third-party crates: ssh / rsync / nvidia-smi are
external binaries invoked through a mockable `CommandRunner` trait.

**No leaky defaults:** the `[dgx]` config table and every `NEWT_DGX_*` env var
default to unset; an unconfigured install never contacts a DGX host.

**Sequence:** 14.1 -> 14.2 -> {14.3, 14.5} -> 14.4 -> 14.6 -> 14.7 -> 14.8 -> 14.9.

## Step 14.1 - dgx config model + endpoint resolution

**Branch:** `step-14.1-dgx-config`
**Touches:** `newt-core/src/dgx.rs` (new), `newt-core/src/lib.rs`,
              `newt-core/src/config.rs`, `newt-core/src/error.rs`.
**Implements:** `DgxConfig` / `DgxNode` / `DgxFormation` / `EndpointKind`;
optional `Config.dgx` (`[dgx]`), back-compatible with dgx-less configs;
endpoint resolution (per-flavor URL env var -> node URL -> `NEWT_DGX_HOST`
host+port synthesis for ollama/vllm); `active_node`, `resolve_endpoint[_for]`,
`resolve_active_model`, `ssh_host`, `ssh_user`, `home_template()`;
`DgxNotConfigured` error wired into `NewtError`. Injectable env (`*_with`) for
deterministic tests.
**Tests:** 24+ - kind parse/serde, node accessor, active-node selection,
resolution precedence, model/ssh chains, home-template + TOML round-trip.
**Mocks:** none (pure data + injected env closures).
**Out of scope:** any CLI surface; Python bindings for the dgx types.
**Estimated diff:** ~560 lines.

## Step 14.2 - `newt dgx` skeleton + `route`

**Branch:** `step-14.2-dgx-cli-route`
**Touches:** `newt-cli/src/dgx/mod.rs` (new), `newt-cli/src/lib.rs`.
**Implements:** nested `Command::Dgx { cmd: DgxCmd }` clap group + dispatch;
`newt dgx route "<task>"` (and `use --for`) reusing `newt_core::Router`
to classify a tier and recommend a formation.
**Tests:** 5+ (`assert_cmd`) - each tier classifies, recommendation output,
unknown-formation handling.
**Out of scope:** network probes, config writes.
**Estimated diff:** ~220 lines.

## Step 14.3 - read-only probes: `status` / `models` / `doctor`

**Branch:** `step-14.3-dgx-probes`
**Touches:** `newt-cli/src/dgx/`, reuse `newt-inference` Ollama/vLLM clients.
**Implements:** `models` (Ollama `/api/tags` + vLLM `/v1/models`), `status`
(endpoint health + `/api/ps` + GPU mem via `CommandRunner`), `dgx doctor`
(probe each configured flavor; surface the .home.lab vs .home.lan DNS note).
**Tests:** 8+ (`wiremock`, incl. HTTPS endpoints; mock `CommandRunner`) -
degrades gracefully when no `ssh_host` is set.
**Out of scope:** config mutation, SSH file transfer.
**Estimated diff:** ~320 lines.

## Step 14.4 - config-mutating: `setup` / `use` / `endpoint` / `formation` / `node`

**Branch:** `step-14.4-dgx-config-cmds`
**Touches:** `newt-cli/src/dgx/`, `newt-core/src/dgx.rs` (atomic save helper).
**Implements:** interactive `setup` (offers `home_template`); `use <model>` /
`use --for`; `endpoint <kind>`; `formation [--list]`; `node add/list/use`.
Atomic write to `~/.newt/config.toml`; non-interactive flags for tests.
**Tests:** 8+ (`tempfile` + `assert_cmd`).
**Out of scope:** SSH, nim.
**Estimated diff:** ~360 lines.

## Step 14.5 - Ollama lifecycle: `pull` / `rm` / `ps`

**Branch:** `step-14.5-dgx-ollama-lifecycle`
**Touches:** `newt-cli/src/dgx/`, `newt-inference` (pull/delete helpers).
**Implements:** `pull` (`/api/pull`, stream progress to stderr), `rm`
(`/api/delete`), `ps` (`/api/ps`).
**Tests:** 5+ (`wiremock`, streaming body).
**Out of scope:** vLLM model management.
**Estimated diff:** ~240 lines.

## Step 14.6 - SSH ops: `run` / `push` + `watch`

**Branch:** `step-14.6-dgx-ssh`
**Touches:** `newt-cli/src/dgx/`, new `CommandRunner` trait.
**Implements:** `run "<cmd>"` (ssh), `push <local> <remote>` (rsync),
`watch` (periodic nvidia-smi refresh). All shelled out via `CommandRunner`,
mocked in tests so no live DGX is required.
**Tests:** 6+ (`mockall`) - arg construction, exit-code propagation,
missing-host error path.
**Out of scope:** interactive SSH sessions.
**Estimated diff:** ~280 lines.

## Step 14.7 - `nim list` / `nim deploy`

**Branch:** `step-14.7-dgx-nim`
**Touches:** `newt-cli/src/dgx/nim.rs` (new), vendored NIM catalog JSON.
**Implements:** `nim list` (catalog) and `nim deploy <model>` (emit a
Kubernetes manifest to stdout).
**Tests:** 5+ (`insta` snapshots of emitted manifests).
**Out of scope:** applying manifests to a cluster.
**Estimated diff:** ~260 lines.

## Step 14.8 - MCP agent tools

**Branch:** `step-14.8-dgx-mcp-tools`
**Touches:** `newt-mcp-server/src/handlers.rs`.
**Implements:** `dgx_gpu_status`, `dgx_run`, `dgx_pull_model` exposed via
`tools/list` + `tools/call`, delegating to the dgx module.
**Tests:** 5+ (in-memory duplex transport + mock `CommandRunner`).
**Out of scope:** non-dgx MCP tools.
**Estimated diff:** ~220 lines.

## Step 14.9 - docs

**Branch:** `step-14.9-dgx-docs`
**Touches:** `README.md`, `docs/`.
**Implements:** DGX section, topology table, `.home.lab` vs `.home.lan` DNS
caveat, example `newt.toml`.
**Tests:** `documentation-audit` pass; no code.
**Out of scope:** n/a.
**Estimated diff:** ~120 lines.

---

# Phase 15 — Role Profiles (persona → tools + caveats + model)

> **PROPOSED — pending roadmap-owner placement.** This phase promotes newt's
> "persona" from a prompt-only system-prompt overlay into a *role profile* that
> also carries a toolset, an agent-bridle capability (caveat) profile, and
> backend/router policy (model/tier). It is the seam the one-airframe-many-roles
> topology (dragon-rider orchestrator, wing-commander arbiter, worker) depends
> on. Step 15.1 lands the type + parser + TUI reporting; everything that *acts*
> on a profile (enforcement, routing, authority minting) is sequenced as
> explicit follow-on steps below. The natural home for the profile-driven config
> at the worker/ACP entry points is Step 9.8's `AgenticConfig` — see the note in
> Step 15.1's out-of-scope list.

## Step 15.1 — RoleProfile in newt-core + TUI wiring

**Branch:** `step-15.1-role-profiles`
**Touches:** `newt-core/src/role_profile.rs` (new), `newt-core/src/lib.rs`
(re-exports), `newt-tui/src/lib.rs` (persona loading + `/persona` handler +
prompt-assembly call site only), `personas/{dragon-rider,wing-commander,worker}.md`
(new templates), `docs/design/role-profiles.md` (new design note).
**Implements:**
- `RoleProfile { prompt, role, tools, caveats, model, tier }` in
  `newt-core::role_profile`, with a parser that splits an **optional** `+++`-fenced
  TOML front-matter block from the markdown body. A file with no front-matter
  parses into an all-`None` profile and behaves exactly like today's prompt-only
  persona (load-bearing backward-compat guarantee).
- `CaveatProfile` (human-friendly serde shape — `"all"`/`"none"`/list per
  filesystem/exec/net axis + `max_calls`) converting into the canonical
  `agent_mesh_protocol::Caveats` via `to_caveats()`, and a `summary()` reporting
  helper. All parsing/conversion/reporting *logic* lives in `newt-core`.
- `newt-tui`: persona loading parses the file into a `RoleProfile` stored on the
  active `Persona` (prompt injected as before); `/persona show` reports role +
  tool allow-list + caveat summary + router policy; new opt-in
  `/persona set <name> --keep-context` swaps the role WITHOUT resetting the
  conversation (persistent-actor principle), default still resets.
- Three shipped templates under `personas/` exercising the front-matter.
**Tests:** newt-core unit tests for parsing (front-matter present / absent /
malformed / unclosed / sparse / empty-block / non-fence), `CaveatProfile`
conversion + sparse-defaults-to-top, `ScopeSpec`/`CaveatProfile` `summary()`
rendering; newt-tui tests for `--keep-context` preserves history vs default
reset, role-bound persona load surfaces tools/caveats and differs from the
prompt-only `coder` default, and all three shipped templates parse into valid
role-bound profiles.
**Out of scope / follow-on steps:**
- **Enforcement** — gating tool dispatch on the profile's `tools`/`caveats` via
  agent-bridle. There is no agent-bridle registry at the TUI dispatch site yet;
  the TUI *records* the active role's tools/caveats but does not enforce them.
  After Step 9.7 relocates the loop to `newt-core::agentic`, enforcement belongs
  in that shared module so it benefits both the TUI and the ACP worker.
- **Router application** — having the router actually honor the profile's
  `model`/`tier` hints (today they are surfaced in `/persona show` only).
- **Worker/MCP entry points minting authority from a profile** — the ACP worker
  / MCP server reading a role profile and minting agent-bridle authority
  (`Caveats`) from `caveats`. The natural carrier is Step 9.8's `AgenticConfig`:
  a role profile would supply the tunables/authority a session embodies.
**Mocks:** none — pure parsing + in-memory persona store over `tempfile` dirs;
no network. (Front-matter fixtures are written to temp dirs in tests.)
**Estimated diff:** ~470 lines newt-core (incl. tests), ~250 lines newt-tui
wiring/tests, 3 template files, 1 design note.

---

# Phase 16 — Mesh Remote Control / Mobile

Design: `docs/design/mesh-remote-control-mobile-app.md` (PR #202, merged). An
Android + iOS app that is itself a first-class agent-mesh peer driving a remote
`newt-agent` over the signed, replay-defended bus — no broker, no SSH-tunnelled
shell. The phone holds a **worker-delegated, attenuated `AgentKey`** (root →
worker → phone); every byte is cryptographically attributable and bounded by
that key's `Caveats`.

This phase is **dependency-ordered**, not strictly sequential — three spikes
gate the build work and must resolve first. Each row is tracked by a GitHub
issue; the UI phases (Android/iOS/WAN/biometric) are roadmap entries that
become issues only **after** the spikes (C, H) answer.

| Step | Deliverable | Issue | Kind |
|---|---|---|---|
| 16.0a | agent-mesh: external-pubkey delegation + envelope signer | agent-mesh#27 | build (in flight) |
| 16.0a′ | agent-mesh: transport external-signer seam (QUIC w/o owned seed) — the `#27` remainder | agent-mesh#28 | build |
| 16.0b | Unify `newt-mesh` onto the `newt-identity` operator root | #228 | build |
| 16.S1 | **Spike:** does the handshake expose a transcript hash for SAS binding? (can invalidate §4.5) | agent-mesh#30 | spike |
| 16.S2 | **Spike:** direct-dial by `(fingerprint, addr)` for WG/WAN | agent-mesh#29 | spike→build |
| 16.S3 | **Spike:** hardware-backed ed25519 on Android/iOS, or software-key fallback | #232 | spike |
| 16.1 | Multi-attach session model — local console as one attachment (gating prereq) | #229 | build |
| 16.1b | `newt/session/v1` + `NewtSessionService` streaming/cancel (reconcile w/ Step 2.3, #167/#123) | #230 | build |
| 16.2 | `newt mesh enroll` + SAS pairing + **revocation MVP** (bundled, not deferred) | #231 | build |
| 16.3 | `newt-mesh-mobile` Rust core + UniFFI "signed echo" on both platforms | #233 | build (new crate) |
| 16.4+ | Android Compose UI · iOS SwiftUI UI · WireGuard WAN · revocation+biometric polish | — | roadmap-only until spikes resolve |

**Sequencing notes:**
- The streaming surface (16.1b / `OutputChunk`) is the **same need** as Step 2.3
  (`ChatChunk`/`ChatStream`) and #167/#123 — land Step 2.3 as the shared
  substrate first; do not build streaming twice.
- 16.S1 (SAS transcript binding) is load-bearing security: if the handshake
  exposes no channel binding, the §4.5 numeric-comparison pairing needs
  redesign. Resolve before 16.2.
- Revocation (`expires_at` + worker denied-fingerprint set) ships **with**
  enrollment (16.2), not at the end — a phone enrolled without revocation is a
  stolen-phone window.
- `newt-mesh-mobile` sits beside `newt-mesh` (out-of-workspace, path-dep to
  agent-mesh) per `mesh_integration.md`, so default-workspace CI stays green.

---

# Phases 17-19 — Context, memory & conversation (hermes-agent learnings)

**Canonical spec: [`docs/design/context-memory-hermes-learnings.md`](design/context-memory-hermes-learnings.md)**
(merged via #243) — gap matrix, per-step contracts, and **§6: ordering is
causal (signed per-writer ticks, BLAKE3 content chains); wall-clock is a
display claim, never an ordering key — binding on every schema below.**
This entry is deliberately thin: the design doc owns the step tables (a
duplicated copy here drifted within hours during review — once is enough).
Benchmarks with a captured baseline gate every phase:
[`docs/testing/context-memory-benchmark.md`](testing/context-memory-benchmark.md)
(baseline = #245, lands before 17.1a).

| Phase | One line | Umbrella issue |
|---|---|---|
| **17** | Durable conversations + recall: SQLite/FTS5 store, §6 causal ordering, workspace identity = blake3(remote+branch), `recall` tool, auto-resume by activity tick | #246 |
| **18** | Token truth + compression v2: prompt-tokens-preferred accounting, structural prune → anchored boundary → redacted summary (summarize, don't discard — retires the #223 class) | #247 |
| **19** | Agent-curated memory: `save_note` tool where the char-cap is the curator, in-band nudge, write-time security scan | #248 |

**Gate:** Step 9.7 (#244) — every †-marked step in the design doc lands inside
`newt-core::agentic` and is blocked on the extraction. Pre-9.7 runway:
17.1-17.4, 17.7, 18.3, 19.1-19.2.

---

# Phase 20 — Model self-tuning (capability feedback loop)

**Canonical spec: [`docs/design/model-self-tuning.md`](design/model-self-tuning.md)**
— field semantics, the per-round observation hook, calibration currencies, and
the out-of-scope fences. A live session refused its own sends over a cached
6,068-token cap while the backend was accepting 8,734-token prompts in the
same transcript — the system held proof its budget was wrong and discarded it,
because the only capability write-back lived in a turn epilogue that an error
skips.

| Step | One line | Umbrella issue |
|---|---|---|
| **20.1** | Passive feedback: per-round `RoundObservation` hook, `max(proven, believed)` send budget, EMA chars/4 calibration, observation-gated epilogue | — |
| **20.2** | Active discovery (shipped): `/probe window` empirical input-boundary binary search → `max_ok_input` at High confidence; cheap `/probe` adds context-window refresh + thinking probe + chars/4 calibration bootstrap; `newt tunings show` staleness markers point stale/unprobed models at `/probe window` | — |
| **20.3** | Fail open for non-authoritative budgets: a send budget resting on the proven-good `max_ok_input` alone (cloud / no-`/api/show` models) may compress but must never *refuse* — anti-thrash dispatches over budget (`DispatchedOverBudget`) and lets the backend rule, so one acceptance raises the HWM out of the spiral. Authoritative ceilings (declared window / `num_ctx` / cw-400 cap / token threshold) still refuse (B6). Fixes the live `gpt-4.1` bail. | — |
| **20.4** | **Progressive-disclosure compaction (spec'd, not built — [`design/progressive-disclosure-compaction.md`](design/progressive-disclosure-compaction.md)).** Compaction as disclosure, not destruction: the `compaction_mode = disclosure` profile knob makes boundary eviction emit a `memory_fetch`-handle index marker (handle + gist) for already-persisted turns instead of a lossy summary; the model faults spans back verbatim on demand. "Paging" is the mechanism metaphor; the concept is progressive disclosure. Reuses the #319 `memory_fetch` / `turn:<conv>#<seq>` substrate — no new store. Always-effective reclaim ⇒ anti-thrash never latches, so it *also* prevents the halt at the source (20.3 is the safety net). Default `summary` stays bit-for-bit until the eval earns a flip. | — |
| **20.5** | Progressive-disclosure durability & thrash control (follow-on): re-page thrash guard (pin recently-faulted spans / page-in budget), durable cross-session disclosure + GC/TTL, and promotion of repeatedly-faulted spans to durable knowledge-bank `note:<id>`. | — |

# Phase 21 — Centaur Data Scientist

**Canonical design: [`docs/design/centaur-data-scientist.md`](design/centaur-data-scientist.md).**
A *Centaur* helper (human-on-top data science: pandas/numpy/EDA), delivered as
an MCP peer so newt stays lean and gilamonster/hermes inherit it unchanged —
engine (`newt-data`) + thin MCP adapter (`newt-mcp-data`) + optional in-notebook
PyO3 submodule. This entry is deliberately thin; the design doc owns the tool
tables. Steps:

- **21.1** `newt-data` skeleton + `DataStore` trait + SQLite ingest/query/summarize. *(done)*
- **21.2** `newt-mcp-data` stdio server with the SQL tools (first shippable Centaur slice). *(done)*
- **21.3** `KernelClient` trait + REST/websocket client + `kernel_attach` / `run_cell`. *(done)*
- **21.4** notebook read / insert / persist-executed-cell + `run_cell(persist_to=…)`. *(done)*
- **21.5** dataframe introspection (`list_dataframes`, `inspect_dataframe`). *(done)*
- **21.6** PyO3 `newt_data` submodule + umbrella/release wiring. *(done)*
- **21.7** interrupt / restart + reconnect hardening.
- **21.8** DuckDB backend behind `DataStore`.
- **21.9** optional raw-ZMQ kernel client.
- **21.10** `docs/decisions/centaur_data_scientist.md` + README config snippet.

# Phase 22 — Hierarchical context scheduler (one engine, many agents)

**Canonical design: [`docs/design/context-scheduler.md`](design/context-scheduler.md).**
Time-share one DGX across many logical agents the way an OS time-shares one CPU.
Layer 1 (vLLM continuous batching + PagedAttention, or Ollama `NUM_PARALLEL`)
already interleaves the GPU; Phase 22 builds Layer 2 — admission + cooperative
scheduling of agent rounds. Two-level hierarchy (dry role names; codenames live
in code comments during dev): **global-scheduler** · **kv-warden** (gate the KV
budget, hold waiters, release on free) · **distributed-scheduler** ·
**execution-worker** (reuses `wyvern-agent`). Tenants live above the scheduler:
**tenant-context** (map-reduce-over-context job) and **per-model-strategy**
(summary/paged/mapreduce, visible/silent). Composes 20.3 (never halt) + 20.4
(swap = paged compaction). MVP co-locates the control tiers in one process over
the serial (Ollama) engine; the concurrent (vLLM) engine and multi-engine split
slot behind the same `Engine` seam later.

- **22.1** `Engine` trait + serial (Ollama) impl; ACB; co-located global-scheduler/kv-warden/distributed-scheduler; cooperative round quantum; round-robin + priority inheritance.
- **22.2** kv-warden KV-budget estimation + hold/release; swap-by-recompute with prefix caching (composes Step 20.4).
- **22.3** tenant-context map-reduce job (carve → submit → reduce) + cross-chunk join pass.
- **22.4** per-model `context_strategy`/`strategy_disclosure` + silent mode; deterministic scheduler mode + live trace.
- **22.5** eval harness (single vs summary vs paged vs mapreduce); wire `auto` to the Phase-20 writeback.
- **22.6** *(follow-on)* concurrent (vLLM) `Engine` impl; KV-offload swap; multi-engine distributed-scheduler split across the bus.

---

# Phase 23 — Crew / Team / Overseer (multi-LLM orchestration)

The **overseer stack**: a conversational model (the overseer) plans, composes a
roster, dispatches crews per plan-step, reviews the returned diff, reports — the
human approves the plan and the roster. All orchestration is pure over three
injected seams (`Dispatcher` + `BackendPool` + `Workspace`), unit-testable with
mocks. **Design of record: [`docs/design/crew-swarm-overseer.md`](design/crew-swarm-overseer.md).**

The keystone: **an agent is the one runtime, scoped** — a crew member (incl. a
resident keysmith/herald) is a Newt/Wyvern *instance* (loadout + tools + caveats +
role), not a program. Capability-as-identity: the caveat spec is both the grant and
the containment.

**Shipped (landed on `main`):**

- **crew** (`newt-scheduler/src/crew.rs`, #425) — one task, roles divide labor
  (planner/navigator/triage); honest `NeedsHumanReview`; CLI front door `newt crew`.
- **panel** (`panel.rs`, #468) — same task to N decorrelated voices, verify-gate
  each, accept by agreement (anti-groupthink).
- **team** (`team.rs`, #474 + per-subtask verify #477) — a lead decomposes a goal,
  a crew runs each subtask over a shared workspace, stops at first block; each
  subtask installs its own `verify`. Live harness: `examples/team_live.rs` (#475).
- **hosted dispatch** (#478) — `BackendKind::Openai` → `/v1/chat/completions` +
  Bearer (`PoolBackend.api_key`); crews/teams run on a hosted LLM, not just Ollama.
- **roster composer** (`roster.rs`, #480) — `compose_roster` surveys live models +
  capability priors → proposes role→model (panel = strongest-per-distinct-family)
  with a rationale, for human approval.
- **agent-callable tools** (#479: surface #482 + impl #484) — the async
  **`CrewRunner`** trait (the universal crew primitive); `compose_roster` + `crew`
  tools behind the **`NEWT_TEAM`** toggle; `LocalCrewRunner` (newt-cli) runs them
  over a `WorktreeWorkspace`, **fails closed** on a read-only session, returns the
  diff for review. The **inversion**: newt-cli owns newt-scheduler + the worktree
  and injects `&dyn CrewRunner` down into the scheduler-free newt-tui loop.

**Authority convergence (knowledge#40 §7.5 · newt-agent#472 · agent-bridle#24).**
The overseer's human gates — "approve the plan", "approve the roster" — *are* §7.5's
`attest` decision surface `{allow, attest, deny}`. Approving a roster **grants a crew
authority**: a policy mutation, and §7.5's keystone is exact — *attenuate freely, but
amplifying (a standing `allow`) requires the human root via `attest`*. So the crew
authority steps build **on the now-merged `attest` primitive** (`agent-bridle#24`:
`Gate::evaluate → Allow | NeedsDischarge | Deny`), not ad-hoc env vars. And `attest`
(signed human acts) shares #472's **root-of-trust bootstrap** with the provenance
plane (#490, signed agent acts) — built once.

**Remaining local steps:**

- **23.1** ✅ **done (#494)** — per-member caveat threading: `run_crew`/`run_team`
  take `&Caveats` and enforce them at the apply step (a member's out-of-`fs_write`
  edits are **refused** — attenuation, never amplify — and fed to triage). Caveats
  travel with the work (the seam for the bridle `Gate` + the remote `CrewTask`).
  *Next within 23.1:* route the shortfall through the Gate's `NeedsDischarge` (folds
  into 23.2's `attest` reframe) rather than the current honest refusal.
- **23.2** *(reframed)* Enabling the crew/team tools **enlarges authority** — a
  standing `allow`, which §7.5 says must be a **live human gesture**, not the
  `NEWT_TEAM` env var (kept only as a dev escape). So the `/team` enable + the
  roster-approval become **`attest` decisions** through `agent-bridle#24`'s `Gate`.
  - **23.2a** ✅ **done (#508)** — the `crew_attest` decision surface in newt-core:
    `crew_step_up_policy()` + `crew_authz(policy, op, resource, established) →
    {Allow, NeedsAttest(Presence)}`, wrapping the now-reachable `step_up` primitive.
  - **23.2b** ✅ **done** — `LocalCrewRunner` consults `crew_authz` on the live
    dispatch path: a `crew`/`team` op is held for an attest when the established
    presence is insufficient. The `/team`/`NEWT_TEAM` enable maps to `Presence::Prompt`
    (structure now), so today it *allows*; a `Passkey`-required op surfaces
    `NeedsAttest` — the real teeth arrive with **BOOT** (#472).
  - *Remaining:* `[team]` config + `[crews.*]` selection; real passkey enforcement
    (BOOT). Until BOOT, this is honest structure, not enforcement.
- **23.3** ✅ **done (#489)** — verified crew work lands as a `crew/<id>` commit on a
  branch in the shared object store (the overseer merges with the embedded `git`
  tool); unverified work is discarded. No file-copy.
- **23.4** Empirical role priors from the rig (#80) feed `compose_roster` (heuristic
  name-based priors today). Independent — anytime.

**BOOT — root-of-trust bootstrap (shared prerequisite, `newt-agent#472`).** A passkey
**seals** the software ed25519 `UserKey` (GitHub anchors identity only). Built **once**;
unblocks BOTH the real `attest` enforcement and the provenance plane. Gated by #472's
red-team **prerequisite fixes** + the real WebAuthn/CTAP2 verifier + the server-side
`pre-receive` hook (the teeth — the client `Gate` is not the boundary).

**Prereq fixes — the unblocked, solo-implementable ones are DONE (all fail-CLOSED now):**

- **§9.3a** ✅ **agent-bridle#25** — bridle MCP boundary: a *missing* grant now defaults
  to DENY-ALL, not `Caveats::top()` (was fail-open). (Follow-up §9.3b: *signed* grant —
  needs the trust root.)
- **§9.1** ✅ **agent-mesh#39** — `CertChain::verify()` fail-closed on causal generation:
  a cert declaring a bounded `valid_for_generation` is refused context-free (was silently
  ignored = fail-open); new `verify_at(generation)` is the checkable path. No wall-clock.
- **§9.2** ✅ **agent-mesh#40** — `delegate_external` now requires **proof-of-possession**
  (`PossessionChallenge` + the subject's signature) before certifying an external pubkey
  (was: certify any pubkey, incl. one you don't control).

**Remaining BOOT prereqs need a DESIGN decision (operator) or are blocked — not solo work:**

- **§9.1 follow-ons** — the KRL (revocation list) + its distribution (a signed mesh topic),
  and a current-generation **source** (none exists yet) + wiring `verify_at` callers. §12
  open questions.
- **§9.3b** signed grant (needs the trust root); **§9.5** online-root SPOF (intermediate-issuer
  split); **§9.4** newt-git push-gate (blocked: no production push site yet).

> **▶ Resume — next session (Monday 2026-06-22): start §11.2 `PresenceCaveats`.** The one
> remaining *unblocked* forward build step: a presence-floor lattice in newt-core, a sibling
> of `GitCaveats`/`SshCaveats` (`meet` = MAX-of-floors, non-amplifying — design §3), carried
> across the mesh envelope. Branch `feat/presence-caveats` off `main`. **Build/push note:**
> the workspace NFS (ontap-sim) can't hold a full build — prefix git push with
> `CARGO_TARGET_DIR=/home/hartsock/.cache/newt-cargo-target` (local 936G disk) so the
> pre-push gate doesn't hit the quota wall. Then §11.3 ceremony → §11.4 `pre-receive` → genesis.

**Follow-on (after BOOT + teeth):**

- **Provenance plane (#490)** — sign config / commands / prompts; the seam is 23.3's
  content-addressed `commit_to_branch` → sign it with the mesh key.
- **`MeshCrewRunner` (#488, remote)** — a resident Newt/Wyvern *parked* on a project
  receives `CrewTask{goal, caveats, workspace_ref}` over agent-mesh and returns
  `CrewResult{diff, status, ledger}`; caveats attenuate per hop (`meet`, never widen)
  so recursion is safe by construction. A `CrewRunner` swap, not a rewrite; remote
  *amplify* needs `attest`, the push-back rides the `pre-receive` teeth. Transport =
  the SSH-CA / iroh dual plane. (wyvern-agent#42.)

---

# Phase 24 — Context-manager reliability + UX (bulletproof summarizer · indicators · configurable managers)

The compression summarizer is the single most failure-prone LLM call in newt and
it fails *silently* into a lossy static marker (observed in the #548 session: a
DGX cold-reload blew the 60s timeout → the conversation middle was replaced by a
one-line marker, irrecoverably). This cluster hardens that call, adds the missing
context-budget TUI indicators, and adds a named-strategy selector seam that
**#546** plugs into. **Umbrella: #559** (decomposition by Beaver). The
`progressive`/`distributed` managers and the retrievable-card store are **owned by
#546** — this phase owns only the reliability (Part 1), the indicators (Part 2),
and the `standard` selector seam (Part 3). *(Numbered 24.x: #559's comment
proposed 23.x assuming 22.x was the top, but Phase 23 is Crew/Team/Overseer —
this is the next free cluster.)* **Design: #559.**

**Reliability (Part 1)**

- **24.1** ✅ **done** — the summarizer request now carries `keep_alive` (mirroring the main loop, threaded from `[tui].keep_alive`) and, for Ollama, **warms the model** (POST `/api/generate` under a 600s timeout) BEFORE the short-timeout summary request, so a cold reload is absorbed off the summary timeout. Best-effort warm (errors ignored). The fix for the #548 eviction/cold-reload timeout.
- **24.2** ✅ **done** — `[tui].summarizer_timeout_secs` (default 60, replacing the hard-coded 60s) + `[tui].summarizer_retries` (default 2): the summary request retries with exponential backoff before giving up to the static marker. Summarizer knobs are consolidated into a `SummarizerOpts` struct (with `Default`) so future knobs don't re-thread the call sites.
- **24.3** ✅ **done** — optional `[tui].summarizer_model`: when the primary model's summary attempts all fail, the summary is retried once on this small/fast model (a rung above the static marker), surfacing the primary error only if the fallback fails too. Added as a `SummarizerOpts.fallback_model` field — no call-site churn.
- **24.4** chunked / hierarchical summarization (segment → summarize segments → summarize the summaries) to attack the OOM root cause; largest reliability PR — ship last.

**UX (Part 2)**

- **24.5** ✅ **done** — `display.rs` gauge formatters: `fmt_token_gauge` (`899k/1024k`), `fmt_tokens_compact` (`1M` = 1024k), `gauge_level` (🟢<75% / 🟡75–90% / 🔴≥90%). Pure + table-driven tests; re-exported from `newt_core::agentic`.
- **24.6** ✅ **done** — the gauge is wired into the RichTUI `header_line`: `set_runtime_context` threads `(used, budget)` (last turn's input tokens vs the resolved send budget = `max_ok_input`/`safe_context`), refreshed per turn, colored by fill. Hidden until the budget is known.
- **24.7** ✅ **done** — the compression notice now renders per-outcome: `✓ summarized` / `⧉ pruned` (amber) and a **loud red `⛔` static-marker last resort** (`summary unavailable — context compacted to a marker; re-read files if needed`) — the #548 silent-context-loss fix. The summarizer also surfaces **live** `↻ retrying (n/m)` and `⚠ falling back to <model>` notices (Option B: they clear the `compressing context…` spinner line and scroll into history, so the recovery is honest).

**Selector seam (Part 3 — `progressive`/`distributed` delegated to #546)**

- **24.8** ✅ **done** — `ContextManager` {standard, progressive, distributed} + `[context].manager` config + `/context manager [name]` command (session override → config → `standard`). Only `standard` is implemented; progressive/distributed report "not yet available (see #546)" and stay on standard. The seam #546 plugs into — its `store-raw-for-lookup` rung is what makes the static marker *recoverable* (deferred retrieval, not loss). Additional modes to add are mined from the research in #13 (paper + `docs/research/notes.txt`).

**Recommended order:** 24.1 → 24.2 → 24.5 → 24.6 → 24.3 → 24.7 → 24.8 → 24.4. (24.1 alone likely eliminates most real-world failures; everything after is depth.) Each step is one PR with the acceptance contract (What / Test plan / Out of scope), a fully-mocked unit tier (wiremock the summarizer endpoint), and the coverage ratchet. Refs: #559 (umbrella), #546 (progressive/distributed substrate + the fallback rung), #548 (surfaced), #166 (DGX OOM history).

---

# Phase 25 — Markdown rendering for assistant output (#568)

Local models emit Markdown; the plain-scroller chat path showed it raw
(`**asterisks**`, pipe tables, ` ``` ` fences). Render it as styled ANSI
**scrolled lines** — never a ratatui/widget surface (the chat path is a plain
scroller per `docs/decisions/plain_scroller_tui.md`), and strippable for the
headless wyvern tier. Engine: `pulldown-cmark` parse + our own crossterm ANSI
emitter (surveyed termimad/tui-markdown/treemd/comrak — all target a widget
surface or a non-CommonMark parser). One PR per step.

- **25.1** ✅ **done** — whole-string ANSI emitter + the `markdown` default
  feature. Inline emphasis (bold/italic/strike/underline), inline code,
  headings, bullet/ordered/task lists with nesting, blockquotes, thematic
  breaks, fenced code (dim, un-highlighted), display-width word wrap. Color-off
  and the `--no-default-features` wyvern strip are byte-for-byte passthrough.
  `render_markdown(src, RenderOpts{color, cols})` in `newt-core::agentic::markdown`.
- **25.2** ✅ **done** — GFM tables: box-drawing (dim borders, bold header),
  per-column display-width fit against `cols` (shrink-widest), left/center/right
  alignment, CJK/emoji width, overflow truncation with `…`. `table.rs`.
- **25.3** ✅ **done** — `MarkdownStreamWriter` block-aware streaming: inline
  lines render per completed line, multi-line blocks (fence/table/list/quote)
  hold until they close, split markers reunite in the line buffer. `stream.rs` +
  the `newt-core` `render_md` example (`--stream`). Wired into `stream_response`
  (the raw per-token `print!` now routes through the writer; the persisted `full`
  stays raw); activation rule is "markdown ⇒ color" until the 25.4 toggle.
- **25.4** ✅ **done** — `[tui].markdown` (`MarkdownMode`) config + a resolved
  `markdown` bool threaded through `ChatCtx` (replacing the 25.3 hardcode) +
  `/markdown [on|off|auto]` command with a per-session override + non-stream
  fallback render; effective = `session ?? [tui].markdown.forced() ?? color`,
  then ∧ color. Cowork driver opts out (renders into its own frame).
- **25.5** ✅ **done** — Wyvern source-tidy: `tidy_markdown_tables(src)` aligns
  GFM table pipes in plain-text source for the headless tier, behind the optional
  `markdown-table-formatter` feature (identity when off; comrak/wasm-bindgen never
  in the default graph). Independent of the `markdown` feature (wyvern strips it).
- **25.6** ✅ **done** — `markdown-syntect` per-language code-block highlighting
  via syntect (foreground-only, theme bg suppressed), behind a feature, off by
  default. Pure-Rust (`default-fancy` = fancy-regex, no onig C dep); not in the
  default graph. `syntect.rs` seam: dim stub without the feature, real
  highlighting with it. **Phase 25 complete.**

---

# Backlog — unscheduled candidates

Filed and triaged, not yet sequenced into a phase. Each entry links the issue and
the threads it must build *on*, so we don't grow parallel mechanisms — promote to a
phase when it's ready to schedule.

- **#565 — project skill-packs** (`enhancement`): first-class support for richer
  skill-pack layouts — `.claude/skills/<name>/SKILL.md` bridge stubs, `loads:`
  frontmatter resolving to source skills, project source trees, and `/skill-name
  args` slash dispatch — without rewriting them into a newt-specific format.
  Build *on*, don't duplicate:
  - **slash dispatch** → the command-plugin runtime's slash-registry
    (`docs/design/command_plugin_runtime.md`); `/skill-name` should route through
    that registry, not a parallel dispatcher.
  - **progressive disclosure** (index-first, bodies on demand, bridge→source) →
    coordinate with #546's `lookup`/skill-base substrate.
  - **authority** → `docs/decisions/ocap_confinement_model.md` + #560: a
    skill-pack's declared permissions/MCP are a *request*, granted only by the
    human; `loads:` must reject paths escaping the pack root (path-fence).
  - extends the existing `docs/decisions/agent-skills.md` foundation.

- **drake-foreman dispatch:** each step's branch is the unit of work. The
  goal posted to drake should be the contents of that step's section. The
  worker LLM produces a patch on a fresh checkout of `main`.
- **Coverage drift:** the first PR (Step 0.3) installs the gate. If a later
  step would drop coverage below 80%, the CI fails and the PR cannot merge.
  This is intentional — it forces tests to land with the code.
- **Sequencing:** Phases run mostly in order. Within a phase, steps are
  usually sequential. Phases 3 and 4 can be parallel; Phases 7 and 8 can
  be parallel; Phase 12 can run in parallel with anything from Phase 8+.
- **PR review:** low-risk steps (most of these — scoped, tested, no infra
  change) follow the workspace policy: auto-merge after CI green if labelled
  `risk:low`. High-risk steps (Phase 0, Phase 11, Phase 12.3) need human
  review.
- **Bookkeeping:** as each step lands, tick it in this file or in the linked
  GitHub Project board (TBD when remote exists).
