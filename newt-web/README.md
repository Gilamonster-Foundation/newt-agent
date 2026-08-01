# newt-web

`newt-web` is newt-agent's server-rendered HTMX cockpit. It composes the
published `TurnDriver` and `ConversationStore` seams; it does not own a second
agent loop.

Conversation output is canonical GFM Markdown. The TUI projects that source to
ANSI, while newt-web projects it to sanitized HTML. Fenced `mermaid` blocks are
progressively enhanced by a pinned, locally served Mermaid 11.15.0 runtime.
Invalid diagrams fall back to their original source, and Mermaid runs with
`securityLevel: "strict"`. The front end has no JavaScript build step.

Run the development server:

```sh
cargo run --manifest-path newt-web/Cargo.toml
```

It listens on `127.0.0.1:8880` by default. See
[`docs/decisions/newt_web_htmx.md`](../docs/decisions/newt_web_htmx.md) for the
auth and deployment posture; a non-loopback deployment must sit behind the
configured gate.

## Headless acceptance

The Playwright harness starts a real newt-web process, a local simulated Ollama
boundary, and headless Chromium. BAT checks the Markdown enhancement contract;
UAT drives a complete prompt/reply/Mermaid flow at a phone viewport.

```sh
cd newt-web
npm ci
npx playwright install chromium
npm run test:bat
npm run test:uat
```

BAT gates pull requests and `main`; both BAT and UAT gate `release/**` branches.
The workflow can also run either tier through `workflow_dispatch`.

Licensed under Apache-2.0. The vendored Mermaid bundle is MIT-licensed; its
license is kept in `assets/mermaid.LICENSE`.
