# newt-agent

Umbrella installer for [newt-agent](https://github.com/Gilamonster-Foundation/newt-agent) (Gilamonster).

```bash
npm install -g newt-agent
newt --help
```

The friendly single-name install. Depends on
[`@gilamonster/newt`](https://www.npmjs.com/package/@gilamonster/newt), which delivers
the prebuilt `newt` Rust binary for your platform via per-platform
`optionalDependencies` (the `uv` / `esbuild` pattern — no postinstall).

The MCP server is a separate install: `npm i -g @gilamonster/newt-mcp-server`.

Other channels: `cargo install newt-agent` · `pip install newt-agent`.

License: Apache-2.0
