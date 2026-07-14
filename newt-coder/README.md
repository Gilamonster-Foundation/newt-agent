# newt-coder

Coder plugin for newt-agent: whole-file emit + server-side diff normalization.

The default `newt worker` path asks the model to emit a unified diff with no
file contents in context — local coder models reliably invent
plausible-but-wrong context lines, so the patch fails to apply (failure mode
T0b). newt-coder is the opinionated fix: it scans the workspace for files the
task mentions, injects them verbatim into the prompt, and asks the model to
emit each updated file in full. `normalize_emission` parses the reply, the
changes are written to the workspace, and the caller captures a real diff via
`git diff`.

Activated via `NEWT_CODER=1` or the per-session ACP param
`{ "coder": true }`; the legacy path remains the default.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
