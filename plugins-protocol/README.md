# plugins-protocol

Newt-Agent provider-plugin JSON-RPC schema + reference client SDK.

Provider plugins run as separate processes and speak JSON-RPC over stdio.
They register opt-in inference backends — most notably the cloud backends
(OpenAI, Anthropic) that the default Newt binary deliberately does not link.

v0 surface: `initialize`, `list_models`, `complete`, `stream`, `shutdown`.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
