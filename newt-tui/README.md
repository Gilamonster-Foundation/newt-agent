# newt-tui

Newt-Agent's terminal-facing chat, input, and active-tool presentation layer.

A lean chat + agentic-coding TUI in the spirit of Codex / Claude Code,
deliberately scoped to chat and agentic coding — not feature-rich. Splash +
chat REPL + slash commands + ocap-gated tool use. Durable configuration lives
in plain `~/.newt/config.toml` (see `newt config`), and the
setup wizards (`newt init` / `newt setup`) probe for local or remote models and
write that file plus one `backends/*.toml` drop-in per endpoint.

## Settings and communication style

### Exact executable grants

An exec permission prompt preserves the executable's full path. Allow once
authorizes that executable for the retry; another executable with the same
name remains outside the grant. A proactive `request_permissions` approval
keeps the same exact target until the matching operation consumes it.
Session and pending one-shot approvals match the exact requested token. A bare
name does not pre-approve an absolute path with the same name; that path needs
its own decision. A different request leaves a pending one-shot grant available.
Previously saved bare-name denials still apply to matching executable names.

`--venv` and `--exec-path` grant the absolute paths of executable files in the
selected directories, including their symlink spellings. Under restricted
exec authority, invoke non-system tools by those absolute paths. Deliberate
bare-name system grants still resolve through the sandbox's trusted system
directories. These grants do not add filesystem or network access, resolve
arbitrary programs from ambient PATH, or make interpreter approval permanent.

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 22:06 EDT | Date: 2026-09-17

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 02:02 EDT | Date: 2026-09-18

### One-shot native filesystem grants

For a native command that needs filesystem access, declare the exact absolute
paths in `run_command.fs_read` or `run_command.fs_write`. Newt asks before
starting that invocation, or consumes a matching pending allow-once approval
from `request_permissions`. An unrelated command leaves the approval pending.
The prompt shows the command context; a later executable or network prompt
retains already-approved filesystem authority for this invocation only.
Existing danger rules, preset and delegation ceilings, and frame isolation
still apply. Later calls need standing authority or a fresh approval.

### Panels

`/settings` is the primary entry point for harness controls: Session,
Permissions, Audit, Backends, Themes, and MCP. Commands such as `/model`,
`/backends`, and `/permissions` remain available as direct shortcuts.

Open `/settings` → **MCP**, or `/mcp`, to manage discovered servers. The list
shows configuration sources, namespace conflicts, connection status, and tool
counts. Enter opens a server, then its tools and each tool's description and
parameter schema. Up/Down navigates; Esc returns one level and keeps selection.
Connection, saved authentication, and OCAP are separate: a connected stdio
server has not necessarily authenticated its upstream services. Tool effect
hints are server metadata, never permission grants.

**Reconnect** recreates the selected connection and refreshes its tools.
**Test** requests tool metadata only; it never calls a tool. A failed test or
reconnect removes stale advertised tools. **Login** uses Newt's existing OAuth
flow for HTTP servers, with the same exact-host permission prompts as startup.
Configured Authorization references remain operator-managed. Stdio servers can
provide an explicit operator login in trusted Newt configuration:

```toml
[[mcp_servers]]
name = "documents"
command = "documents-mcp"
login_argv = ["documents", "login"]
```

The panel confirms the program and literal arguments before lending it the
terminal. Successful login reconnects the MCP child so it can read updated
credentials; failed or interrupted login leaves the connection unchanged.
Borrowed project/Claude configuration and server metadata cannot supply login
commands. Without `login_argv`, log in through the server's CLI and select
Reconnect. Lean and nonterminal sessions retain the `/mcp` text commands.

Open `/settings` → **Themes** (or press `t` at the settings index) to select
**newt**, **daylight**, **phosphor**, or one of your saved themes.
Up/Down chooses a field; Left/Right changes it.
Choose a role to edit its color, bold, dim, italic, underline, reverse, or
strikethrough. The color field also accepts `#rrggbb` or an ANSI index `0–255`.
The fixed preview shows headings, file names, human text, thinking, agent
replies, and spill output while you edit. Enter accepts a typed color; Enter
again applies and remembers the theme. Esc discards the unapplied draft and
returns to the settings index; Esc there returns to chat. To keep a named copy,
type a Save name and press Ctrl-S, then Enter to apply. Built-in names are
protected; saved custom names can be updated.

Themes live under `~/.newt/themes/`; `active.toml` stores the last applied
selection. Changes affect subsequent output immediately; committed terminal
scrollback retains the colors it had when printed. `NEWT_THEME` color overrides
still apply at startup. The default uses light-blue headings, bright-white
inline code/file names, cyan human prompts, gray agent replies, and dim-gray
thinking and spill text. Edit `thinking` independently from `agent-text` to
distinguish reasoning from replies. The thinking style covers streamed and
folded reasoning and its closing label; the `accent` role colors the background
activity indicator. Other editable roles include `human-text`,
`markdown-heading`, `inline-code`, and `spill`.

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

File edits in the rich surface show NewtUI's numbered added and removed rows,
with syntax foregrounds and semantic backgrounds, in committed scrollback and
the existing completed spill viewport. Resizing reprojects the captured model;
the normal scroll, expand and dismiss controls retain ownership. Theme overrides
`added`, `removed`, `added-background`, and `removed-background` tune these colors.
For example, `NEWT_THEME="added-background=#183825,removed-background=#411d20"`
sets both source backgrounds. The rich build includes the existing Syntect
syntax/theme assets; lean builds keep the plain text receipt path. The `/spill`
archive still retains bounded text; typed full inspection and exact export are
subsequent work, as recorded in
[Step 13.3](../docs/ROADMAP.md#step-133--rich-file-change-cells-in-scrollback-and-the-completed-viewport).

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

## Remembering permissions

Open `/settings` → **Permissions (OCAP)** to choose the terminal approval
default or **Make session allows permanent**. The default is **Allow once**;
save **Deny** to require an explicit approval choice each time. Enter confirms
the displayed choice. Closing the prompt, losing input, or running without an
interactive terminal does not approve a request. The setting grants no
authority by itself and is saved as `[tui.permissions].prompt_default`.
When that setting is absent, MCP connection prompts retain the older
`mcp_net_prompt_default` preference; other terminal approvals use Allow once.

Permanent promotion shows every current session allow and asks **Are you sure?**
with cancellation selected. Only that reviewed snapshot is saved; once-only
answers and previously loaded permanent approvals are excluded. The grant
kinds remain distinct: executable, file read, file write, network host, remote
tool, and git write. Recall checks current denials before applying permission
ceilings. A recalled directory or executable-basename grant that covers a denied
target is withheld entirely because the scope cannot express an exclusion;
independently configured authority is unchanged.
If any entry cannot become standing authority, Newt refuses the whole save
and explains why. Disabling new permission prompts still honors verified
permanent approvals; requests without an existing grant remain denied.

The hostname prompt's **[A] Allow permanently** remains a separate legacy
action: it appends that network host to TOML configuration. Use the Permissions
panel or `/permissions save` for the reviewed, signed and encrypted snapshot.

The encrypted store is `ocap/session-grants.age` beside the selected durable
configuration file. Its content-addressed payload is signed by the local
operator key and bound to the canonical workspace. Newt verifies the entire
existing store before merging a confirmed snapshot and atomically replacing
the ciphertext. Wrong keys, invalid signatures, damaged files, or missing
keys fail closed; existing data is not replaced with an empty policy. Startup
loads keys without creating them. A first confirmed save may create an
encryption identity only when no store exists.

Keep both the signing key and encryption identity: losing either prevents
loading existing approvals. Removing the encrypted store revokes its approvals
on the next load; restart running sessions to discard their cached approvals.
This does not remove grants from ordinary configuration or other policy files.
There is no automatic live revocation between processes or protection against
restoring an older, valid encrypted store. Anyone holding both private keys can
create valid records.

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

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: Shawn Hartsock | Time: 22:51 EDT | Date: 2026-09-16

Model: GPT-6 | Harness: Codex CLI v0.154.0 | Operator: S Hartsock | Time: 19:26 EDT | Date: 2026-09-17
