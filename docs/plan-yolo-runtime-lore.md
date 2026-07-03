# Plan: Inject yolo runtime lore into model context

## Problem statement

In a `newt --debug --trace --yolo` session, the model claimed that
`run_command` was blocked by the brush/agent-bridle shell dependency even though
`--yolo` was active. That is stale operational lore. The shipped
`--disable-ocap` / `--yolo` path routes permitted `run_command` calls through
the unconfined host shell while native fs tools remain workspace-fenced and
`web_fetch` remains net-leashed.

The human-facing startup banner already says this. The model-facing context does
not make the same runtime fact hard enough to miss.

## Current code facts

- `newt-core/src/agentic/tools.rs::ocap_disabled()` returns true only when
  `NEWT_DISABLE_OCAP=1`.
- `run_command` checks the named-permission exec floor, then takes the
  `host_shell_dispatch` bypass when `ocap_disabled()` is true and the floor
  permits the command.
- `newt-tui/src/lib.rs::ocap_disabled_banner()` tells the operator that commands
  run unconfined on the host shell under `--disable-ocap`.
- Existing docs still contain useful long-term brush-shell context, but weak
  models can over-apply that context and report the wrong active runtime mode.

## Goal

When `--disable-ocap` / `--yolo` is active, every model turn should include a
short, authoritative runtime note:

```text
Runtime authority: --disable-ocap/--yolo is active. run_command uses the
unconfined host shell when the active exec floor permits it, not the
brush/agent-bridle confined shell. Do not claim run_command is unavailable due
to brush in this mode. Native fs tools remain workspace-fenced; web_fetch
remains net-leashed.
```

This note should be per-session runtime context, not persistent memory.

## Non-goals

- Do not widen default authority.
- Do not make `web_fetch` ignore the net allowlist under `--yolo`.
- Do not change the native fs tool workspace fence.
- Do not delete the brush-shell design docs; they remain correct for confined
  shell goals and non-yolo behavior.

## Implementation plan

1. Find the model message assembly point that already injects environment,
   workspace, or tool-context lore.
2. Add a small helper that returns the runtime authority note only when
   `NEWT_DISABLE_OCAP=1`.
3. Inject the note near the tool descriptions or environment block so it is seen
   before tool selection.
4. Keep the human banner unchanged, but make the model-facing text match its
   authority semantics.
5. If `run_command` fails, prefer error text that names the active dispatch path
   (`host shell` vs `agent-bridle shell`) so recovery does not invent the wrong
   cause.

## Regression coverage

- Unit-test the helper: flag off returns no note; flag on returns the yolo
  authority note.
- Add a prompt/message assembly test proving the note is present in model input
  when `NEWT_DISABLE_OCAP=1`.
- Keep or add an execution-path test proving a permitted command under yolo goes
  through `host_shell_dispatch`.
- Add a negative assertion that the yolo runtime note does not say `web_fetch`
  is unconfined.

## Acceptance

- In `newt --trace --yolo`, the sent model context contains the runtime
  authority note.
- A model asked to push or run a shell command under yolo is not given stale
  context that implies brush blocks `run_command`.
- Existing yolo behavior stays intact: host-shell exec, fenced native fs tools,
  and net-leashed `web_fetch`.
