# Headless docking drive harness

Drives the **real** newt TUI (in a detached `tmux` session) and the **real**
newt-web cockpit against **one shared `ConversationStore`**, backed by a stub
Ollama — no human, no real model, no network. It exists so the docking
lifecycle (`dock` / `undock` / `multi-dock` / `select`) can be exercised
**headlessly** as it is built, per `docs/decisions/newt_web_docking.md`.

## Run

```sh
# build the cockpit once (excluded crate, own target dir)
CARGO_TARGET_DIR=~/.cargo-target/newtweb cargo build --manifest-path newt-web/Cargo.toml
# then drive it
newt-web/tests/drive/drive.sh
# pin binaries (e.g. a feature-branch build) with:
NEWT_BIN=/path/to/newt WEB_BIN=/path/to/newt-web newt-web/tests/drive/drive.sh
```

Exits non-zero on any failed assertion (CI-gate-able). Self-cleaning: the
`tmux` session, the cockpit + stub processes, and the temp store are all torn
down on exit.

## What it proves today

- `newt` reaches an idle chat prompt in `tmux` and completes a turn against the
  stub (claims a conversation in the store).
- The cockpit, pointed at the **same** store, lists that session — the
  multi-session **select** / overview.
- A prompt **injected through the web** lands in the store inbox (204), and the
  TUI stays the sole writer (D2 mirror + inject).
- The **Phase-1b idle-wake gap** is reproduced headlessly: while the TUI sits
  idle at `❯` the inject is *not* consumed; it drains at the next turn boundary
  (a keypress). This `PENDING` line flips to `PASS` when Phase 1b lands.

## Pieces

- `stub-ollama.mjs` — deterministic offline Ollama stand-in. Answers
  `/api/tags`, `/api/show` (advertises a large context window so newt's prompt
  fits), and `/api/chat` with a reply that **echoes the last user message**, so
  a driver can assert a specific (possibly injected) prompt reached the model.
- `drive.sh` — the orchestrator + assertions. Helper functions
  (`tui_send` / `tui_wait` / `tui_capture`, `web_get` / `web_post`, store
  probes) are the reusable driving surface; the `dock` / `undock` /
  `multi-dock` legs are explicit `PENDING` slots that light up as Phases 2–5
  land.

## The tmux driving pattern

`tmux new-session -d` → `send-keys` to type → `capture-pane -p` to read/assert.
A real terminal is required because the behaviors under test (idle-wake, lines
printed through the line arbiter) are properties of an actual TTY — the same
reason `prompt_visibility_test` drives a real PTY.
