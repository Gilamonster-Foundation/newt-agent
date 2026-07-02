# Plan: Fix prompted permissions and editing reliability

## Problem statement

The captured `newt --trace` session shows two user-visible failures:

1. A `web_fetch` net denial renders the allow/deny menu, but the user cannot
   actually use it as a working permission prompt.
2. The model keeps exploring and narrating instead of making the requested code
   edit, even after planning tools are available.

This plan keeps the changes small and testable. It does not start from the
transcript's inferred `harness.rs` location, because this repo currently routes
the relevant code through `newt-core/src/agentic/tools.rs`,
`newt-core/src/agentic/mod.rs`, and `newt-tui/src/lib.rs`.

## Current code facts

- `web_fetch` already has a permission pre-check in
  `newt-core/src/agentic/tools.rs`: it parses the host, checks
  `caveats.permits_net(&host)`, and calls `PermissionGate::ask` when the host is
  outside the current net scope.
- The TUI gate implementation is `PromptPermissionGate` in
  `newt-tui/src/lib.rs`. The prompt text is built by `permission_prompt_text`;
  the actual production reader is `prompt_permission_choice`.
- `prompt_permission_choice` prints the menu and then calls
  `io::stdin().read_line`.
- During an agent turn, `run_chat` wraps `chat_complete` in
  `with_interrupt_watch`. On Unix this enters cbreak mode and starts a sibling
  thread that polls and reads from `STDIN_FILENO` to detect Esc and Ctrl-C.
- That creates a likely stdin ownership bug: the permission prompt is trying to
  read a line while the terminal is in non-canonical mode and another thread is
  concurrently reading from the same fd.
- `edit_file` and `write_file` exist, are advertised, and are routed through
  `execute_tool`. The transcript's "cannot edit" failure is therefore likely
  loop steering/recovery, not a missing editor tool.
- The read-only nudge exists in `newt-core/src/agentic/mod.rs`, but the
  captured session still let the model burn many rounds on broad searches,
  hallucinated paths, and summaries.

## Goals

- Permission prompts are real operator interactions, not just text emitted into
  the transcript.
- `web_fetch` denials can be allowed once or for the session in an interactive
  TUI session, and still fail closed in headless/eval/ACP contexts.
- File editing tools remain governed by the same permission system, and a denied
  `edit_file` or `write_file` prompt can also be answered reliably.
- The agent loop pushes weak local models toward `edit_file`/`write_file` after
  enough evidence has been gathered, instead of producing a final "I could not
  find it" answer while tools remain available.
- Each part has focused tests and can be reviewed independently.

## Non-goals

- Do not widen default authority or bypass ocap enforcement.
- Do not make headless runs block on human input.
- Do not remove Esc/Ctrl-C interrupt support.
- Do not replace the whole TUI input system in one PR.
- Do not tune one model-specific prompt as the main fix.

## Part 1: Add focused regression coverage for the permission seam

Target files:

- `newt-core/src/agentic/tools.rs`
- `newt-tui/src/lib.rs`

Work:

1. Add or tighten a core test that proves `web_fetch` with a gate asks for
   `DenialKind::Net` and target `github.com` before dispatch.
2. Add a TUI-level unit test around `PromptPermissionGate::ask` that confirms
   the prompt text for `web_fetch` net denial includes the grant menu and that
   choices map to records and minted caveats.
3. Add a sibling test for an `edit_file` or `write_file` `fs_write` denial so
   the permission prompt path is not only covered by network fetches.
4. Keep tests pure where possible: scripted `ask_human` closures are fine for
   gate behavior; no real network and no real terminal for unit tests.

Acceptance:

- Tests prove the core gate is requested for net and fs denials.
- Tests establish the intended behavior before touching terminal input logic.
- `cargo test -p newt-core web_fetch_gate`
- `cargo test -p newt-tui permission`

## Part 2: Make permission prompts own stdin while they are active

Target file:

- `newt-tui/src/lib.rs`

Likely root cause:

`with_interrupt_watch` enters cbreak mode and spawns `watch_for_interrupt`, which
polls and reads stdin. `prompt_permission_choice` then calls `read_line` from the
same stdin. This means a permission answer can be consumed by the watcher, or
`read_line` can observe EOF/partial behavior under non-canonical terminal mode.

Work:

1. Introduce a small "prompt section" guard used by `prompt_permission_choice`
   and `prompt_user_input`.
2. While the guard is active, suspend the interrupt watcher from reading stdin.
   A small shared atomic state is enough: `Running`, `PromptActive`, `Stopping`.
3. Restore canonical line mode for the prompt, read the answer, then restore the
   cbreak settings used by the interrupt watcher.
4. Ensure the guard is exception-safe through RAII so terminal state is restored
   even if the prompt reader returns an error.
5. Keep non-Unix behavior as a no-op wrapper around the current read path.
6. Add a small debug trace line only under existing trace/debug mode if useful:
   "permission prompt opened" and "permission prompt answered", without logging
   the user's raw answer.

Implementation notes:

- Prefer passing a tiny `PromptIo` or `InterruptCoordinator` into
  `PromptPermissionGate` over using global state.
- If threading the coordinator through `PromptPermissionGate` is too large for
  one patch, first split `prompt_permission_choice` behind an injectable reader
  so later PRs do not need to touch gate policy.
- The watcher must not treat normal prompt input like `a`, `s`, `d`, or newline
  as interrupt bytes.

Acceptance:

- In an interactive run, `web_fetch https://github.com/...` outside the net
  allowlist accepts `a`, fetches once, and does not add a session grant.
- The same prompt accepts `s`; a second fetch to the same host does not prompt.
- `d` denies once; `D` denies for the rest of the session.
- Esc/Ctrl-C still interrupts a normal model turn outside active prompts.
- Headless tests still receive no prompt gate.

## Part 3: Add an integration-style terminal test for prompted permissions

Target files:

- `newt-tui/src/lib.rs`
- A new test helper under `tests/` if the existing test layout cannot host a PTY
  test cleanly.

Work:

1. Add a Unix-only PTY test for the prompt reader/coordinator. It should run the
   prompt path with a pseudo-terminal, write `a\n`, and assert `AllowOnce`.
2. Add a second PTY test that starts the interrupt watcher, enters prompt mode,
   writes `s\n`, and proves the watcher does not consume it.
3. Keep the test narrowly scoped to input coordination. Do not require a real
   model backend or real `web_fetch`.
4. If PTY tests are too slow or flaky for the default suite, gate them behind a
   feature or mark the pure coordinator tests as the default gate and document
   the PTY test command.

Acceptance:

- The stdin race is covered by a test that would fail with concurrent watcher
  reads.
- The test does not make CI depend on external network or a real terminal.

## Part 4: Improve denial recovery messages for model action

Target files:

- `newt-core/src/agentic/tools.rs`
- `newt-core/src/agentic/mod.rs`

Problem:

If a prompt is denied or unavailable, the model sees an error string and may
continue broad searching or summarize failure. It needs a clear next action:
call `request_permissions`, change approach, or edit with the available tools.

Work:

1. For denied `web_fetch` after the human chooses deny, return a model-facing
   denial that distinguishes "operator denied" from "no gate available" and from
   "net leash denied".
2. Preserve the security property: the model never gets to self-grant. The
   message can tell it to call `request_permissions` only when a gate exists or
   when the session can support one.
3. Add a failed-call steering case for repeated broad `find`/`grep` searches
   that return truncated or unhelpful results. The steering should recommend
   targeted `read_file` on known candidate files and then `edit_file`.
4. Treat "read_file failed because path does not exist" as a signal to search by
   symbol/text, not to keep rereading the same missing path.

Acceptance:

- Repeated failed or empty search calls are short-circuited with actionable
  guidance.
- Permission denial messages remain honest and do not imply authority was
  granted.
- Existing tests for `request_permissions` and denial formatting still pass.

## Part 5: Make "must edit now" an explicit loop state after planning

Target file:

- `newt-core/src/agentic/mod.rs`

Problem:

The captured session shows the model using planning tools and then continuing
to inspect or summarize instead of editing. The current read-only nudge fires
after three read-only rounds, but it is advisory and can be lost during
compaction.

Work:

1. Add a stronger action nudge when the active plan has an implementation step
   in progress and the model has already read candidate files.
2. The nudge should be specific: "You have enough context. Call `edit_file` or
   `write_file` now, or state the exact blocker."
3. Persist this state through compaction by including it in the existing
   plan/state reseat path instead of relying only on a transient user message.
4. Do not force edits for review/explanation tasks. Gate it on task shape:
   explicit "make the change", active implementation plan, and candidate files
   already read.
5. Add tests using a mock responder that keeps reading after the stronger nudge;
   assert the nudge is injected and repeated only as designed.

Acceptance:

- A model that reads for multiple rounds after an implementation plan receives
  a stronger action instruction.
- Review-only and plan-only requests do not receive edit pressure.
- The existing read-only nudge tests continue to pass.

## Part 6: Add an end-to-end scripted reproduction

Target files:

- `newt-core` tests for loop behavior
- `newt-tui` tests for prompt behavior
- Optional `docs/testing/` fixture or script

Scenario:

1. Start from a workspace with no net allowlist for `github.com`.
2. Model calls `web_fetch` for a GitHub URL.
3. TUI prompt receives `a`.
4. Tool retries or dispatches under widened caveats.
5. Model reads the relevant local file.
6. Model calls `edit_file`.

Work:

1. Mock `agent_bridle` or isolate the `web_fetch` gate pre-check so no real
   network is required.
2. Use a scripted model responder in `newt-core` to force the same sequence of
   tool calls as the transcript.
3. Assert the final workspace file changed and the permission decision was
   recorded.
4. Add a negative variant where the operator answers `d`; assert no network
   dispatch under widened caveats and the model receives denial guidance.

Acceptance:

- The failure mode from the transcript has a deterministic regression test.
- The test verifies real file mutation via `edit_file`, not just final text.

## Part 7: Documentation and operator affordances

Target files:

- `README.md` or `docs/ocap/permissions-facade-design.md`
- `/help` text in `newt-tui/src/lib.rs` if behavior changes are user-visible

Work:

1. Document that prompted permissions are on by default for interactive TUI
   sessions and off for headless/eval/ACP.
2. Document the exact choices: allow once, session allow, deny once, deny always.
3. Explain that high-danger grants may refuse session allow.
4. Add a troubleshooting note: if no prompt appears, check TTY status and
   `--no-prompt-for-permissions` / `NEWT_NO_PROMPT_FOR_PERMISSIONS`.
5. Keep the doc clear that `web_fetch` still uses the net leash after a grant.

Acceptance:

- `/permissions` and `/help` match actual behavior.
- Operator docs do not imply that `--yolo` disables `web_fetch` net checks.

## Suggested PR order

1. `step-XX.1-permission-seam-regressions`
   - Pure tests around current `web_fetch`, fs denial, and gate behavior.
2. `step-XX.2-prompt-stdin-ownership`
   - Fix prompt input coordination with the interrupt watcher.
3. `step-XX.3-pty-prompt-regression`
   - Add Unix PTY regression coverage for the actual input race.
4. `step-XX.4-denial-recovery-steering`
   - Improve model-facing denial and repeated-search guidance.
5. `step-XX.5-plan-action-nudge`
   - Make implementation plans push toward real edit tools after enough context.
6. `step-XX.6-scripted-session-regression`
   - End-to-end scripted reproduction covering prompt plus edit.
7. `step-XX.7-docs-permission-ux`
   - Update operator docs and help text.

Each PR should include "What this PR does", "Test plan", and "Out of scope" in
the PR body, and should preserve the repo acceptance contract.

## Test plan for the full series

Run the focused tests after each PR:

```bash
cargo test -p newt-core web_fetch_gate
cargo test -p newt-core read_only_nudge
cargo test -p newt-tui permission
```

Before merging the final PR in the series, run the repository contract:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
just cov-ci
```

## Risks and mitigations

- Risk: pausing the interrupt watcher could make Ctrl-C unresponsive during a
  prompt. Mitigation: prompts are short, and Ctrl-C can be parsed by the prompt
  reader as deny/cancel if needed.
- Risk: terminal modes are not restored after an error. Mitigation: use RAII
  guards and PTY tests.
- Risk: stronger edit nudges push models to edit too early. Mitigation: gate on
  implementation intent plus candidate-file evidence, and keep review/plan-only
  tasks excluded.
- Risk: denial recovery text over-promises permission availability. Mitigation:
  distinguish interactive gate present, gate absent, and operator denied.

## Definition of done

- The GitHub `web_fetch` denial from the captured session produces a usable
  prompt in an interactive TUI.
- Answering the prompt changes execution: allow fetches under widened caveats,
  deny fails closed, session choices persist for the process.
- The model can proceed from permission resolution to a real `edit_file` or
  `write_file` call in a scripted regression.
- The permission system remains off for headless/eval/ACP and never widens live
  session authority directly.
- The full acceptance contract is green.
