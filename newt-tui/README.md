# newt-tui

Newt-Agent TUI surfaces (ratatui): code mode + pilot mode.

A lean chat + agentic-coding TUI in the spirit of Codex / Claude Code,
deliberately scoped to chat and agentic coding — not feature-rich. Splash +
chat REPL + slash commands + ocap-gated tool use. It is NOT a settings UI:
configuration is plain `~/.newt/config.toml` (see `newt config`), and the
setup wizards (`newt init` / `newt setup`) probe for local models and write
that file.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
