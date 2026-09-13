# newt-tui

Newt-Agent's terminal-facing chat, input, and active-tool presentation layer.

A lean chat + agentic-coding TUI in the spirit of Codex / Claude Code,
deliberately scoped to chat and agentic coding — not feature-rich. Splash +
chat REPL + slash commands + ocap-gated tool use. Durable configuration lives
in plain `~/.newt/config.toml` (see `newt config`), and the
setup wizards (`newt init` / `newt setup`) probe for local or remote models and
write that file plus one `backends/*.toml` drop-in per endpoint.

## Settings and communication style

Open `/settings` → **Themes** to select **newt**, **daylight**, **phosphor**, or
one of your saved themes. Up/Down chooses a field; Left/Right changes it.
Choose a role to edit its color, bold, dim, italic, underline, reverse, or
strikethrough. The color field also accepts `#rrggbb` or an ANSI index `0–255`.
The fixed preview shows headings, file names, human and agent text, and spill
output while you edit. Enter applies and remembers the theme; Esc discards the
draft. To keep a named copy, type a Save name and press Ctrl-S, then Enter to
apply. Built-in names are protected; saved custom names can be updated.

Themes live under `~/.newt/themes/`; `active.toml` stores the last applied
selection. Changes affect subsequent output immediately; committed terminal
scrollback retains the colors it had when printed. `NEWT_THEME` color overrides
still apply at startup. The default uses light-blue headings, bright-white
inline code/file names, cyan human prompts, gray agent replies, and dim-gray
spill text. These are editable semantic roles, including `human-text`,
`agent-text`, `markdown-heading`, `inline-code`, and `spill`.

The rich TUI offers `/settings`, `/backends`, `/model` (also `/models`), and `/psyche` controls.
The model picker supports arrow-key selection. Only resident models show a
`[loaded]` badge, to the right of the name. The active session model uses the
`active-model` theme color; the badge uses `loaded-model` (for example,
`NEWT_THEME='active-model=cyan,loaded-model=magenta'`). On llama.cpp routers,
`l` loads, `u` confirms unloading, `x` confirms unloading other models and loading
the highlighted one, and `r` refreshes. Enter selects the model for chat.
In `/backends`, Down opens the selected backend's editor; Left/Right on its model
field cycles through models discovered from that endpoint. Enter saves the backend
configuration. Use `/model` to change the model for the active conversation.

The rich panels share NewtUI's key and close vocabulary. Newt retains event
decoding, terminal ownership, settings validation and writes. The
[first adoption step](../docs/ROADMAP.md#step-131--shared-panel-key-and-close-vocabulary)
records the immutable dependency pin and the boundary for later component moves.

`/psyche` includes independent agreeableness, extraversion, warmth,
approachability, and prosocial-behavior dials. Select `steady`, `direct`, or
`sociable`, adjust values from 0 through 100, and use `auto` to inherit a
persona's value. Enter applies the draft; Esc cancels. `:w <name>` saves a named
persona, while `:wq <name>` saves and applies. Existing names need explicit `!`
overwrite. Style remains separate from cognition, tenacity, and tool authority.

`/posture <name>` and `/settings posture <name>` are that tool-authority axis,
and they share one resolved skill/framing plus an optional permission floor.
Each accepted turn uses one snapshot for its prompt and enforced caveats.
Invalid bindings leave the previous posture intact and report an error; `off`
removes only the posture floor, never the underlying session restrictions. The
binding survives conversation and persona changes within the session.

See [personality and named personas](../docs/guide/personality.md) for tab-local
overrides, save/reload behavior, and the meaning of each preference.

## Context summaries

Context summarization uses the shared progress row and can be interrupted while
a provider's compaction is pending. `timeout_secs` in `summarizer.toml` also applies to
the embedded engine, including model loading. It is a per-summary-request limit,
not a deadline for an entire multi-chunk compaction. Embedded cancellation is
cooperative: the caller returns while a synchronous load or forward may still be
finishing, and another embedded generation is refused until that worker exits.

The turn footer reports `awaiting operator` when smart-harness adjudication
classifies a question, and `incomplete` when narration ends without an answer.
The question or observed reply remains available; these status notices stay
separate from model text.
The durable smart harness currently requires confined Linux execution with
Landlock. Native [macOS support](https://github.com/Gilamonster-Foundation/newt-agent/issues/2281)
and [Windows support](https://github.com/Gilamonster-Foundation/newt-agent/issues/2282)
are tracked separately; enabling it there currently fails startup.
On a supported host, enable `[smart_harness] enabled = true` to use durable frames
and an independent CPU auxiliary. Smart mode selects from retained originals and disables legacy
history and close-time summaries; each turn prints its frame head for inspection
or resume. See [configuration and limits](../docs/guide/smart-harness.md).

Each conversation retains its embedded auxiliary and the exact model/tokenizer
bytes pinned when that conversation opens. Later turns reuse those assets after
checking the current authority, configuration, and primary protocol. Replacing
or removing the original files does not change a live conversation; a new
conversation loads and pins the currently selected files. Changing the selection
or authority requires a new conversation. External auxiliaries continue to
revalidate configured credential availability on every turn.

## Tool output spills

Every completed tool call is rendered through the same bounded spill block;
`[tui] spill_lines` sets its row count and defaults to 3. The default
`newt-agent` build also enables a TTY-only live viewport for streaming shell
output. Each tool starts collapsed and follows the tail while it runs, with
Up/Down moving through retained output. Space or Enter toggles the boundary
control: `⧉` expands from the configured height up to the terminal's safe row
budget, and `▣` collapses it again. The thumb disappears whenever all retained
lines fit.

`/spill <N>` changes the row count for the current session, `/spill reset`
restores configuration, and `/spill 0` disables the live viewport while
keeping unbounded completed output. Live interaction requires Unix plus both
stdin and stdout attached to terminals. Pipes, `TERM=dumb`, unsupported
platforms, and builds without the `live-spill` feature stay completion-only and
emit no viewport cursor controls.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
