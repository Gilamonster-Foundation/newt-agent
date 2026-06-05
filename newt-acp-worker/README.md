# newt-acp-worker

Newt-Agent ACP worker — Agent Client Protocol server for drake-foreman
dispatch.

Speaks the [Agent Client Protocol](https://agentclientprotocol.com) over
stdio so a foreman can dispatch coding goals to Newt instances. The worker
contract:

- Worker ONLY edits files; never `git add` / `git commit` / `git push`.
- An empty `git diff` after a turn is a deterministic crash, counted against
  the model's scorecard.
- `TaskReply.model_id` is mandatory.

Also exposes diff capture helpers, worker identity/caveat handling, and
Prometheus metrics.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
