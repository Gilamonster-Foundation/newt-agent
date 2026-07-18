# newt-tui

Newt-Agent's terminal-facing chat, input, and active-tool presentation layer.

A lean chat + agentic-coding TUI in the spirit of Codex / Claude Code,
deliberately scoped to chat and agentic coding — not feature-rich. Splash +
chat REPL + slash commands + ocap-gated tool use. It is NOT a settings UI:
configuration is plain `~/.newt/config.toml` (see `newt config`), and the
setup wizards (`newt init` / `newt setup`) probe for local or remote models and
write that file plus one `backends/*.toml` drop-in per endpoint.

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
