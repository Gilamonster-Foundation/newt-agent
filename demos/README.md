# Settings walkthroughs

These are operator-driven walkthroughs of Newt's settings systems. The
production-workflow recordings are not generated yet; this page does not
present the generic newtui recordings as proof of Newt's integrations.

Use a disposable workspace and configuration when exploring changes. Screens
depend on the build's features and available backends: an unavailable panel or
model must explain that limitation, not silently choose something else.

## Session settings and presentation

Open `/settings`. Explore the session fields and the presentation controls:
editor bindings, reasoning display, Markdown, input prompt, and output detail.
Change a value, cancel out of the Session section, and then leave the settings
shell itself. Reopen to check it was not applied. Apply a change, finish leaving
the shell, then reopen `/settings` and inspect Audit for its settings receipt.
Audit is a snapshot taken when the shell opens; the write happens after the
shell closes. Leaving a section alone returns to the index.

The deep-link form, such as `/settings prompt`, reaches the same setting; it is
not an independent preference. A session override, a receipt, and a saved
configuration are different things. Do not infer persistence across restarts
merely because a value appears in the session panel.

Inspect `/spill status`, compare `/spill summary` with `/spill excerpt`, then
use `/spill reset` to release both the row-count and summary-mode session
overrides. On a Rich TUI build with `live-spill`, `/spill last` or
`/spill open <id>` opens a retained completed result; use a displayed result id
and show the refusal when the result or viewer is unavailable.

Read more: [terminal UI](../newt-tui/README.md).

## Backends and models

Open `/backends` to inspect the configured routes. Preview another choice and
cancel; the active backend should remain unchanged. Use `/models` to inspect
the served models, and `/model <name>` to select one actually offered by that
backend. An unreachable backend or unavailable model is part of the demo,
not a reason to substitute a different route without telling the operator.

Where the panel offers editing a backend drop-in, save and chooser cancellation
are separate operations: a completed file save survives subsequently leaving
the chooser. Inline entries that cannot be edited should say so.

The summarizer has separate settings: inspect `/summarizer show`, then use
`/summarizer timeout 30` and `/summarizer retries 1` in the disposable
configuration. These write `summarizer.toml` immediately, without an apply/cancel
draft. Recheck `/summarizer show` and the file; the running chat retains its
startup summarizer configuration, so restart the chat process to use those changes.
This walkthrough does not provision or download a model.

Read more: [setup and backend configuration](../docs/guide/setup.md).

## Psyche and personas

Open `/psyche` to preview a persona and its cognition and tenacity dials. Cancel
the draft and check that the active values are unchanged. Reopen, edit, and
save under a disposable name using `:w <name>`; saving a persona file does not
by itself apply the draft. `:wq <name>` saves and applies. An existing name is
refused unless overwrite is explicitly requested.

List saved choices with `/persona list`. Select a saved persona with
`/persona set <name> --keep-context` to preserve the conversation. Persona
selection can also carry backend and capability metadata; it is not just a
display name.

Use newly named disposable personas in this save walkthrough. After saving,
reload the file and check the persona's prose, tool list, skills, and caveats
as well as the edited dials. The saver preserves parsed metadata and prose,
not the original TOML comments or formatting. An unavailable persona refuses
saving instead of replacing a profile whose restrictions are unknown.

The five personality rows control agreeableness, extraversion, warmth,
approachability, and prosocial behavior. Start with `steady`, `direct`, or
`sociable`; compare an explicit zero with `auto` inheritance. The recording
must include save/reload/reselection and show that editing style retains the
persona's other fields and restrictions. Switch tabs to check isolation, then
clear the persona and inspect the new conversation's empty persona selection
and released session style overrides.

Read more: [personality controls](../docs/guide/personality.md),
[role profiles](../docs/design/role-profiles.md), and
[config-panel interaction](../docs/decisions/harness_config_panel.md).

## Permissions and posture

Use `/permissions` and `/permissions audit` for inspection. Those views do not
grant access. Use `/posture <name>` to apply an available named restriction,
then inspect its status. `/posture off` removes that named restriction; it does
not remove the underlying session authority limit.

A recording of the corrected settings/posture integration must wait for that
repair to land. Approval examples must use disposable resources and explicitly
distinguish allow-once from session approval. Never use real secrets or wider
authority merely to make a demonstration proceed.

Read more: [operating modes and permission postures](../docs/decisions/operating_modes_and_permission_postures.md).

## Context controls

Start with `/context` and `/context stats`, then inspect the available feature,
size, and compaction controls through `/context feature`, `/context size`, and
`/context compaction`. This is a text workflow, not a separate context panel or
save dialog. Show the resolved state after a change and after releasing a
session override; include the refusal for a feature absent from the build.

Inspect `/context manager`, then compare the session selections
`/context manager standard` and `/context manager append-only`.
`/context manager progressive` demonstrates the unavailable-manager refusal;
check that the prior selection remains active. Manager changes do not save
configuration, and choosing `standard` is a selection, not a reset to config.

Read more: [terminal UI context controls](../newt-tui/README.md).

## Recording contract

Following the [newtui tape convention](https://github.com/Gilamonster-Foundation/newtui/blob/main/demos/README.md),
recordings must be reproducible from scripted keys and a declared terminal
size. Use the actual Newt routes, panels, setters, and persistence with an
isolated configuration and a deterministic loopback backend. Only backend
responses may be simulated; do not draw a replacement UI for the recording.

Each workflow must show its cancellation or refusal path as well as applying a
change. Assert rendered landmarks and saved state before accepting an artifact.
Keep credentials, personal paths, private network details, and real user
conversation content out of both recordings and fixture files.
