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
- **`platforms.json`** lists the supported release targets: `darwin-arm64`,
  `linux-x64` (glibc), and `win32-x64`. `sync-versions.mjs` stamps exact pins at
  release, after checking the version against `Cargo.toml`.
- **Process forwarding.** The shim passes arguments and standard streams to
  the binary and preserves its exit status. On Unix it forwards `SIGINT`,
  `SIGTERM`, and `SIGHUP`; Windows uses Node's native process semantics.

## Scope (this PR)

Ships **`newt` + `newt-mcp-server`** — the two binaries `release.yml`'s
`build-binaries` actually produces (`-p newt-agent -p newt-mcp-server`). The
`@gilamonster/newt-mcp-data` and `@gilamonster/newt-provider-openai` names are
reserved but not shipped here — wire them in once those binaries are added to the
release build. Intel macOS, Linux ARM64, and musl builds are outside this matrix.
The package-manager abstraction and `newt upgrade` from #1221 are separate work.

## Develop

```bash
just npm-test              # Node 22; no npm install or Rust build required
```

The same suite runs through `just check` and CI on Linux, macOS, and Windows.
It checks resolution, process execution, version refusal, and real `npm pack`
contents for both binaries on every declared platform. Unix signal tests are
explicitly skipped on Windows. Tests use temporary fixtures and no registry calls.

## Release

The [release workflow](../.github/workflows/release.yml) validates npm inputs
before `build-binaries`, then wraps its accepted artifacts without rebuilding.
Platform packages publish first; only after all six succeed do the two scoped
shims and `newt-agent` umbrella publish. Existing published versions are skipped;
other publication errors fail the job. Stable versions use `latest`, prereleases
use `next`. A `v` tag must match the Cargo workspace version exactly; versions
with `+build` metadata are refused because npm normalizes that metadata away.
Checked-in `0.0.0` package versions are placeholders replaced during release.

Publishing requires [npm trusted publishers](https://docs.npmjs.com/trusted-publishers/)
on all nine packages, configured for `Gilamonster-Foundation/newt-agent` and
`release.yml`, with direct publishing enabled. Stage-only authorization cannot
run this workflow's `npm publish`. Registry setup is an operator prerequisite;
the test suite does not verify it. The jobs use GitHub-hosted runners, Node 22
(at least 22.14), npm 11 (at least 11.5.1), and `id-token: write`. They publish
only on version tags and do not use a stored npm token.
