# Decision: Agent Skills (agentskills.io-compatible)

**Status:** Accepted (MVP)
**Date:** 2026-06-03
**Tracking issue:** Gilamonster-Foundation/newt-agent (see PR link)
**Relates to:** `docs/decisions/agentic_object_capability_security.md`
(the leash/ocap substrate skills compose with) and
`docs/decisions/conversation_context_architecture.md` (skills are
on-demand context, not always-on context).

---

## TL;DR

A **skill** is procedural knowledge the agent loads *on demand* — "how this
project writes commits", "how we cut a release", "how to drive tool X". We
adopt the [agentskills.io](https://agentskills.io) format so a skill is a plain
folder on disk that is **portable** across newt-agent, hermes-thoon, and
Anthropic's Claude Code with zero translation:

```
~/.newt/skills/
  commit-style/
    SKILL.md          # YAML frontmatter + Markdown body
    template.txt      # optional bundled files (scripts, templates, …)
  release-checklist/
    SKILL.md
```

The agent only ever sees the **index** (one `name: description` line per skill)
in its system prompt; the full body loads only when the model calls the
`use_skill` tool. This is **progressive disclosure** — the prompt stays small
no matter how many skills are installed.

## The format

A `SKILL.md` is `---`-delimited YAML frontmatter followed by a Markdown body:

```markdown
---
name: commit-style                 # required
description: How this repo writes commit messages.   # required
when_to_use: Before authoring any git commit here.    # or `triggers:`
version: 1.0.0                     # optional
license: Apache-2.0               # optional
caveats:                          # optional — agent-mesh Caveats serde shape
  exec: { only: ["git"] }
  fs_read: all
  fs_write: { only: ["/repo/CHANGELOG.md"] }
  net: { only: [] }
  max_calls: { at_most: 5 }
---
Use the imperative mood. Wrap the body at 72 columns. …
```

Frontmatter schema:

| field         | req | meaning                                                        |
|---------------|-----|----------------------------------------------------------------|
| `name`        | yes | unique skill id; the folder name and the `use_skill` argument  |
| `description` | yes | one line shown in the index                                    |
| `when_to_use` | no  | when the agent should reach for it (alias: `triggers`)         |
| `version`     | no  | semantic version                                               |
| `license`     | no  | SPDX id                                                        |
| `caveats`     | no  | declared capability set (see below) — **parsed, not yet enforced** |

The `caveats` block mirrors the field set of
`agent_mesh_protocol::caveats::Caveats` — `exec` / `fs_read` / `fs_write` /
`net` (each a `Scope`: `all` or `{ only: [...] }`) and `max_calls` (a
`CountBound`: `unlimited` or `{ at_most: N }`). It is the ocap hook: a skill
can declare the narrowest authority its scripts need.

Bundled files (any sibling of `SKILL.md`) are listed by `discover` and surfaced
to the model when the skill loads, so a skill can ship scripts and templates.

## Host-scoped, not per-workspace

Skills live under `$HOME/.newt/skills`. Installed skills are the operator's
trusted procedural knowledge and should be available to every session,
independent of which workspace newt is launched in. A missing directory is the
common "no skills installed" case and is treated as *empty*, never an error.

## Architecture

- **`newt-skills`** — a deliberately light crate (`serde`, `serde_yaml`,
  `anyhow`; no `tokio`, no network). It owns the format:
  - `Skill { name, description, when_to_use, version, license, body, dir, files, caveats }`
  - `Skill::parse(text, dir)` — splits the leading `---` frontmatter from the
    body (robust to BOM / leading blank lines / a missing trailing newline)
    and deserializes the frontmatter via `serde_yaml`, with a *clear* error on
    a missing/unterminated fence or malformed YAML.
  - `discover(dir) -> Vec<Skill>` — scans `<dir>/*/SKILL.md`, lists bundled
    sibling files, sorts by name, and silently skips a broken skill so one bad
    folder can't hide the rest.
  - `index_block(&skills)` — the progressive-disclosure index (names +
    descriptions only).
  - `load_body(dir, name)` — the `use_skill` payload: the body plus bundled
    file paths.
- **`newt-tui`** wires three things:
  1. **Index injection** in `build_system_prompt_with_soul`: if any skills
     exist, an `Available skills (call use_skill to load one):` block of
     one-line entries is appended. ONLY names + descriptions — never bodies.
  2. **`use_skill` tool** in `tool_definitions()` (input `{ "name": string }`),
     advertised on both the Ollama and OpenAI-compatible tool-call paths.
  3. **Dispatch** in `execute_tool`: `use_skill` reads `~/.newt/skills` and
     returns the named skill's body + bundled file paths.

## Leash composition

Skills compose with the bridle leash (`docs/decisions/agentic_object_capability_security.md`):

- **`use_skill` is a read of trusted config**, not an exec of arbitrary code, so
  it is **not** leash-gated.
- **A skill's bundled SCRIPTS run via `run_command`**, which is already
  agent-bridle's confined shell under the session `[tui].permissions` caveats.
  So skill scripts are governed by the leash *today*, with no extra wiring — a
  skill cannot escape the session's authority just by shipping a script.
- **Per-skill `caveats` enforcement is a documented FOLLOW-UP.** The intended
  behavior is: when a skill loads, **meet** its declared `caveats` into the
  session authority (`Caveats::meet`) for the duration of that skill's work, so
  a skill can only ever *attenuate* (narrow), never amplify, the session's
  capabilities. The MVP **parses** the `caveats` block and exposes it on
  `Skill::caveats` but does not yet meet it into the live session.

## Scope

**In this MVP:**
- `newt-skills` crate: parser, discovery, index, `use_skill` payload.
- Progressive-disclosure index in the TUI system prompt.
- `use_skill` tool on both backend wire paths.
- Parsing (only) of the optional `caveats` frontmatter block.

**Out of scope (follow-ups):**
- Per-skill caveats **meet-enforcement** into the live session.
- A `/skills` slash command (list/inspect installed skills).
- Skills **shipped with** newt (bundled defaults).
- An `install`/`add` command for fetching skills.
- Per-workspace skill overlays.

## Why this shape

- **Portability over a bespoke format.** agentskills.io is the lingua franca
  emerging across harnesses; reusing it means a skill authored once works in
  newt, hermes-thoon, and Claude Code. Tools are telescopes — the user's
  procedural knowledge is the sky, and it should not be locked into newt.
- **Progressive disclosure keeps context cheap.** Bodies can be long; only the
  index is always-on. The body is paid for only when actually used.
- **The leash already covers scripts.** Because `run_command` is the confined
  shell, we get script-level confinement for free; the only new ocap surface is
  the per-skill `caveats` meet, which we stage as a follow-up rather than
  rushing enforcement.
