# newt-agent npm packages

Sources for newt-agent's npm distribution — Rust binaries delivered to npm users
the way `uv`, `esbuild`, and `@biomejs/biome` do (no Rust toolchain, no `pip`),
ported from the [scrybe](https://github.com/hartsock/scrybe) reference shim
(newt-agent#1221).

| Path | Package | Role |
|---|---|---|
| `newt-agent/` | [`newt-agent`](https://www.npmjs.com/package/newt-agent) | Unscoped umbrella — `npm i -g newt-agent` → the `newt` CLI. Depends on `@gilamonster/newt`. |
| `newt/` | `@gilamonster/newt` | `newt` CLI bin shim. Lists per-platform binaries as `optionalDependencies`; execs whichever npm resolved. |
| `newt-mcp-server/` | `@gilamonster/newt-mcp-server` | `newt-mcp-server` bin shim (same pattern). `npm i -g @gilamonster/newt-mcp-server`. |
| *(generated)* | `@gilamonster/<bin>-<os>-<arch>` | Per-platform packages carrying just the prebuilt binary + `os`/`cpu` fields. Built by the release job. |

## Design

- **Name-derived resolver.** `newt/lib/binary.cjs` and `bin/run.cjs` are **generic** —
  each shim derives its binary name and platform-package names from its *own*
  package name, so the exact same two files are shipped by every `@gilamonster/<bin>`
  shim. Add a binary = one `package.json` (+ a copy of `bin/`, `lib/`, `platforms.json`).
- **No `postinstall`.** The binary arrives as a normal optional dependency —
  hermetic, offline-cacheable.
- **`platforms.json`** is the single source of truth (`darwin-arm64`, `darwin-x64`,
  `linux-x64`, `win32-x64`); `sync-versions.mjs` stamps exact pins at release.

## Scope (this PR)

Ships **`newt` + `newt-mcp-server`** — the two binaries `release.yml`'s
`build-binaries` actually produces (`-p newt-agent -p newt-mcp-server`). The
`@gilamonster/newt-mcp-data` and `@gilamonster/newt-provider-openai` names are
reserved but not shipped here — wire them in once those binaries are added to the
release build.

## Develop

```bash
cd npm && npm test          # node --test: manifest integrity + resolver + happy-path exec
```

Publishing uses npm **OIDC trusted publishing** (see `.github/workflows/release.yml`
`build-npm` + `publish-npm-meta`). Each package has a trusted publisher configured
on npmjs pointing at `Gilamonster-Foundation/newt-agent` → `release.yml`.
