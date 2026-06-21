# Design: the command-plugin runtime (`~/.newt/commands/<name>/`)

**Status:** Design / proposed (Shawn Hartsock, 2026-06-21)
**Related:** `docs/design/transparent_command_layer.md` (the parse·route·govern
model this realizes), `docs/decisions/structural_parsing_over_regex.md` (AST not
regex at boundaries), `docs/decisions/agentic_object_capability_security.md`
(OCAP), issue #548 (`/help` rollups — the command list this de-clutters),
`newt-tui::forge_context` + `newt-core::forge_resolvers` (the forge resolver,
which becomes the first command-plugin).

---

## TL;DR

Slash commands become **installable plugins** living on disk under
`~/.newt/commands/<name>/`, so a command — `/issue` first — can be
**installed, removed, updated, or replaced without rebuilding newt-agent**. A
plugin is *data + a declared kind*, never arbitrary code: a `manifest.toml`
names the command and binds it to a **compiled handler "kind"** (e.g.
`forge-fetch`), plus kind-specific config files. This keeps the OCAP boundary
intact — you instantiate vetted kinds, you don't smuggle ambient code in through
config.

## Why

- Today slash commands are a compiled `match` in `dispatch_slash`. Adding,
  removing, or tweaking one (even just its help text or which forges it knows)
  needs a rebuild. That doesn't scale, and it clutters `/help` (#548).
- The forge resolver already proved the model: its *behavior* is config
  (`issues.toml`). Generalize that one rung — let the **command itself** be
  config too — and the whole command surface becomes pluggable.

## Layout

```
~/.newt/commands/
  issue/
    manifest.toml         # identity + kind + help + authority
    resolvers.toml        # kind-specific config (the forge resolver specs)
  ticket/                 # a user-added command, no rebuild
    manifest.toml
    resolvers.toml
```

One directory per command. Drop a directory in → the command exists. Delete it →
it's gone. Swap files → updated/replaced. The directory name is the default
command name (overridable in the manifest).

### `manifest.toml`

```toml
name    = "issue"                 # the slash command (/issue); defaults to dir name
kind    = "forge-fetch"           # which COMPILED handler runs this command
help    = "pull a github/gitlab/jira issue/PR/MR into the chat context"
aliases = []                      # optional extra names
enabled = true                    # soft on/off without deleting

# OCAP — the authority this command may exercise (see the tiering in
# transparent_command_layer.md). The runtime enforces it; a command can never
# exceed its kind's tier or this declared scope.
[authority]
tier         = "egress"           # read | egress | exec
egress_hosts = ["github.com", "gitlab.com", "*.atlassian.net"]  # allow-list

# Kind-specific config: each kind documents the files/keys it reads.
[config]
resolvers = "resolvers.toml"
```

`resolvers.toml` for the `forge-fetch` kind is exactly the `[[resolver]]` format
already specified (host/path/command/fallback/show/token) — the current
`issues.toml` content, relocated under the command that owns it.

## Command kinds (the compiled boundary)

A **kind** is a Rust handler compiled into newt; a **plugin** is a config
instance of a kind. This split is the security keystone:

- Kinds are vetted code with a fixed authority tier. The initial kind:
  - **`forge-fetch`** — parse a URL structurally, match a resolver by host, run
    its CLI (REST fallback), pull the result into chat context. Tier: `egress`,
    bounded by `egress_hosts`. (This is the `forge_context` engine.)
- Future kinds (each its own design + review): `shell-route` (route a CLI like
  `git` to the embedded/confined tool — #552), `http-fetch`, `note-template`.
- There is deliberately **no `exec-arbitrary` kind**. You cannot define a
  command that runs ambient code through config — that would hand the plugin
  surface the very ambient authority OCAP forbids. Anything execution-shaped
  routes through the confined executor under a caveat, or not at all.

## Lifecycle

```
newt commands list                 # installed commands, kind, source, enabled
newt commands install <src>        # copy a command dir into ~/.newt/commands/<name>/
newt commands remove <name>        # delete (or set enabled=false)
newt commands update <name> <src>  # replace files in place
newt commands show <name>          # manifest + resolved config + authority
```

- **Install** is an explicit, human act — adding a command is *granting a
  capability*, so it is never auto-fetched or model-initiated.
- **Built-ins ship as default plugins.** newt carries an embedded default set
  (e.g. the `issue` command); a user directory of the same name overrides it.
  This means even built-ins are inspectable, removable, and replaceable — and
  the built-in `match` shrinks to a small set of truly-core commands.
- A malformed manifest disables *that* command with a logged warning; it never
  breaks startup or the other commands.

## Loading + dispatch

1. At session start, scan `~/.newt/commands/*/manifest.toml` (+ the embedded
   defaults), validate each, and build a **command registry**: name/alias →
   (kind handler, config, authority).
2. The input loop resolves a typed `/name args` against the registry first; a
   match routes to the kind's handler with the command's config + an
   authority guard. Unmatched falls through to the small compiled core
   (`/help`, `/exit`, …).
3. `/help` is generated *from the registry* (manifest `help`), which is also the
   #548 rollup: one line per command, `/<name> help` expands.

## OCAP discipline

This runtime is an authority surface, so it obeys the same rules as the
transparent command layer:

- **Installing = granting.** The user placing a command dir is the capability
  grant. The model cannot install, enable, or invoke an install.
- **Manifest declares, runtime enforces.** A `forge-fetch` command may egress
  *only* to its `egress_hosts` allow-list, matched structurally against the
  parsed host (never regex). A command cannot exceed its kind's tier.
- **User-invoked vs. model-invoked.** A human typing `/issue` exercises the
  user's authority; if a command is ever exposed to the model, that path is
  leashed by a caveat (deny-by-default), per the two-owners rule.
- **Tokens stay out of context.** `token_env`/`token_file` are read by the
  handler to authenticate; only rendered text reaches the model.
- **No ambient escape hatch.** No kind grants unconfined exec; that is the line.

## How the current work maps on

The green forge-resolver PR is the substrate, refactored onto this layout:

| Now (PR) | Under this design |
|----------|-------------------|
| hardcoded `/issue` in `dispatch_slash` | the `issue` **default plugin** (manifest, kind `forge-fetch`) registered from the registry |
| `~/.newt/issues.toml` | `~/.newt/commands/issue/resolvers.toml` |
| `forge_context` engine | the `forge-fetch` **kind** handler |
| `[tui] resolve_forge_urls` | a manifest/option on the `issue` command |

So nothing is wasted: `forge_context`/`forge_resolvers` become the first kind +
default plugin; the new work is the registry, the manifest loader, the dispatch
integration, and the `newt commands` lifecycle CLI.

## Phasing

1. **Manifest + registry + loader** — parse `~/.newt/commands/*/manifest.toml`,
   build the registry, validate authority. (Pure/testable.)
2. **Dispatch integration** — route `/name` through the registry; `/help` from
   the registry (closes #548).
3. **`forge-fetch` kind = the issue command** — port the existing engine behind
   the kind interface; ship `issue` as the default plugin.
4. **`newt commands` lifecycle CLI** — list/install/remove/update/show.
5. **Further kinds** — `shell-route` (#552 `git`), etc., each its own review.

## Decisions (resolved 2026-06-21)

1. **Built-in defaults: github + gitlab, embedded and overridable.** The
   `github`/`gitlab` resolvers ship *embedded* in newt (compiled defaults), not
   materialized to disk — clean upgrades, nothing to migrate. A user directory
   of the same name overrides them. (Jira is **not** a universal built-in: its
   host is per-tenant `*.atlassian.net`, so it ships as a documented
   user-installed example with the tenant filled in.)

2. **Per-command `resolvers.toml`, layered over the built-in base — no shared
   user pool.** Each command directory is self-contained; its resolver file
   *extends/overrides* the embedded github/gitlab base. Rationale: a shared pool
   would (a) blur the OCAP boundary — the command's `egress_hosts` allow-list
   would have to filter the pool anyway, so the pool only adds a way to
   over-grant; (b) break atomic install/remove — "delete the directory = gone"
   stops being true if resolvers live in a shared file; (c) couple commands.
   The only upside (DRY for common forges) is already covered by the embedded
   defaults. If a *custom* resolver ever needs sharing, add an explicit
   `inherits = ["<name>"]` opt-in later — isolation stays the default.

3. **Provenance / signing of installed commands** (for shared/team
   distribution) is deferred to its own OCAP design —
   **Gilamonster-Foundation/newt-agent#560**. Local single-user installs trust
   "the user put it there"; verifiable distribution needs that design first.
