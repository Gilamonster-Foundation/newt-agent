# newt-skills

Agent **skills** with **capability caveats** — a Rust library for discovering,
parsing, and installing [agentskills.io](https://agentskills.io)-format skill
folders, extended so a skill can declare the authority it needs.

A *skill* is procedural knowledge an agent loads on demand: a plain folder on
disk holding a `SKILL.md` (YAML frontmatter + a Markdown body) and any bundled
files. That folder format is portable — the same skill works in Claude Code,
Codex, and the Gilamonster agent line — so this crate is usable by any harness
that speaks the format, not just newt-agent.

## What makes this different: caveats

An off-the-shelf `SKILL.md` is just a prompt fragment: knowledge with no bound
on what it may *do*. This crate extends the frontmatter with an optional
`caveats` block that attaches **object-capability attenuation** to the skill —
the commands it may run, the paths it may read or write, the hosts it may reach,
the number of tool calls it may make:

```yaml
---
name: deployer
description: Ship the release.
caveats:                        # object-capability attenuation
  exec: { only: [git, cargo] }  # may run only git and cargo
  fs_write: { only: [dist] }    # may write only under dist/
  net: all                      # unrestricted network
  max_calls: { at_most: 5 }     # at most five tool calls
---
Run the release checklist, then tag and push.
```

Each axis is a lattice value whose top is "unrestricted", so a skill can only
ever *narrow* authority, never widen it. Because the caveats live in the skill
file itself, the capability contract travels **with** the skill instead of in
harness-specific configuration. That is the crate's reason to exist:
*agentskills.io-compatible skills, with capability caveats.*

> The caveats block is parsed today; *enforcing* it — meeting it into the live
> session's authority when the skill loads — is the documented follow-up. The
> value now is that the contract is captured, portably, in one file.

## Quick start

```rust
use newt_skills::{discover_paths, index_block, load_body_from};
use std::path::PathBuf;

// An ordered search path — earlier directories win a name collision.
let roots = [PathBuf::from("./skills"), PathBuf::from("/etc/agent/skills")];

// Build the progressive-disclosure index for a system prompt: one
// `name: description` line per skill, never the body.
if let Some(index) = index_block(&discover_paths(&roots)) {
    println!("{index}");
}

// Load one skill's full body on demand (e.g. from a `use_skill` tool call).
let body: String = load_body_from(&roots, "deployer")?;
```

Parse a `SKILL.md` directly and read its caveats:

```rust
use newt_skills::{Skill, SkillCountBound};

let skill = Skill::parse(skill_md, "")?;
if let Some(caveats) = &skill.caveats {
    assert_eq!(caveats.max_calls, Some(SkillCountBound::AtMost(5)));
}
```

(The runnable, tested version of the caveats example lives in the crate-level
rustdoc.)

## Discovery, precisely

- **`discover(dir)`** scans one root: `<dir>/*/SKILL.md`, sorted by name.
- **`discover_paths(&dirs)`** scans an ordered search path and deduplicates by
  name, **earlier directory wins**. `discover_paths_with_shadows` also returns
  the losing duplicates so a CLI can warn about them.
- Hidden entries (`.git`, `.DS_Store`, editor swap files) are skipped. A
  subdirectory without a readable, parseable `SKILL.md`, or one whose declared
  name is unsafe, is skipped silently so one broken skill can't hide the rest.
- A missing or unreadable directory is not an error — it yields no skills.

Progressive disclosure: only the index (names + descriptions) goes in the
prompt; a body loads only when asked for by name.

## Names are a security boundary

A skill name selects a folder, and the name an agent asks to load is
model-controlled input. `validate_skill_name` rejects anything that is not a
single safe path component — `..` traversal, path separators, hidden `.`-names,
control bytes — so a name can never escape a search root or an install
destination. Discovery drops skills whose declared name fails that check, and
`load_body_from` rejects an unsafe request *before* touching the filesystem.

## Errors

Every fallible call returns the typed `SkillError`, separated by concern:

| Concern | Variants |
| --- | --- |
| Authoring (`SKILL.md` is wrong) | `Frontmatter`, `Yaml` |
| Identity (name rejected / absent) | `InvalidName` (with a matchable `NameRejection`), `UnknownSkill` |
| Environment (the filesystem said no) | `Io` (path-tagged), `DestinationExists`, `Unsupported` |

`SkillError` implements `std::error::Error`, so `?` still lifts it into
`anyhow` or any `Box<dyn Error>`.

## Testability

Discovery is written against a small `SkillFs` seam (`OsFs` is the real disk),
so the discovery and traversal-guard logic is unit-tested against an in-memory
filesystem — no disk, deterministic — while a thin real-filesystem tier grounds
those mocks against actual symlink and permission behavior.

## Dependencies

`serde` + `serde_yaml` (pure-Rust YAML frontmatter) and `thiserror`. No async
runtime, no C dependencies.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
