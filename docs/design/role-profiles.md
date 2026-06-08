# Role Profiles — persona as a prompt + toolset + caveats + router policy

Status: spike (additive vertical slice)
Crates touched: `newt-core` (new `role_profile` module), `newt-tui` (wiring)
Related: agent-bridle caveats (`agent_mesh_protocol::Caveats`), `newt-core::Tier`

## Problem

A "persona" in newt is, today, a **system-prompt overlay** and nothing more:

- `newt-tui`'s `Persona { name, prompt, path }` loads `~/.newt/personas/{name}.md`
  and injects the file's text after the soul in `build_system_prompt_with_persona`.
- `/persona set <name>` swaps the overlay and **resets** the conversation.

That is enough to change how the model *talks*, but the one-airframe-many-roles
topology needs a role to change what it can *do*. A `dragon-rider`
(orchestrator), a `wing-commander` (arbiter/judge), and a `worker` are the same
newt binary wearing different authority, tools, and routing — not just different
prose. This is the seam the whole fleet topology depends on.

## Design

Promote the persona into a **role profile** that binds, in one file:

1. a **prompt** (the markdown body — unchanged),
2. a **role** name,
3. a **tool allow-list**,
4. a **capability (caveat) profile** — an agent-bridle `Caveats` shape, and
5. **router policy** — a preferred `model` and/or `tier`.

### File format (backward compatible)

A role profile is still a plain `.md` file. The only addition is an **optional**
TOML front-matter block fenced by `+++` lines at the very top of the file:

```markdown
+++
role = "worker"
tools = ["read_file", "write_file", "run_command"]
model = "qwen2.5-coder:14b"
tier = "STANDARD"

[caveats]
fs_read = "all"
fs_write = ["src/", "tests/"]
exec = ["cargo", "git"]
net = "none"
max_calls = 80
+++

# Worker

You edit files and run builds...
```

**Backward-compat guarantee (load-bearing):** a `.md` file with *no* `+++`
front-matter parses into a `RoleProfile` whose every field except `prompt` is
`None`. It behaves *exactly* like today's prompt-only persona. The `+++` fence
must be the first line; `+++foo` or a leading blank line means "no
front-matter".

### Caveat shape

`[caveats]` maps onto the canonical lattice type
`agent_mesh_protocol::Caveats` (re-exported as `newt_core::Caveats`). Each
filesystem/exec/net axis is a `ScopeSpec`:

- `"all"` → `Scope::All` (unrestricted, the top of that axis)
- `"none"` → `Scope::Only({})` (authorizes nothing)
- `["a", "b"]` → `Scope::Only({a, b})` (explicit allow-list)

An **omitted** axis defaults to `"all"`, matching `Caveats::top()`, so a sparse
profile attenuates only the axes it names. `max_calls` maps to `CountBound`
(`AtMost(n)` when set, else `Unlimited`). `valid_for_generation` is not
expressed in the file — it is minted at dispatch time (causal, never
wall-clock), so the profile always converts to `Scope::All` on that axis.

### Tier

`tier` is the existing `newt_core::Tier` (`FAST`/`STANDARD`/`COMPLEX`/`REVIEW`,
serialized UPPERCASE).

## API

`newt-core::role_profile`:

```rust
pub struct RoleProfile {
    pub prompt: String,            // markdown body (front-matter stripped)
    pub role: Option<String>,
    pub tools: Option<Vec<String>>,
    pub caveats: Option<CaveatProfile>,
    pub model: Option<String>,
    pub tier: Option<Tier>,
}

impl RoleProfile {
    pub fn parse(text: &str) -> anyhow::Result<Self>;  // splits +++ front-matter
    pub fn is_role_bound(&self) -> bool;               // false for prompt-only
}

pub struct CaveatProfile { /* fs_read, fs_write, exec, net: ScopeSpec; max_calls: Option<u64> */ }
impl CaveatProfile { pub fn to_caveats(&self) -> Caveats; }

pub enum ScopeSpec { Keyword(ScopeKeyword), Items(Vec<String>) }   // "all"/"none" or [..]
pub enum ScopeKeyword { All, None }
```

Re-exported at the crate root: `RoleProfile`, `CaveatProfile`, `ScopeSpec`,
`ScopeKeyword`.

## What this slice wires

- `newt-core`: parse + represent a role profile; convert the caveat shape into
  the canonical `Caveats`. Unit-tested (front-matter parsed; absent → all-None;
  malformed → clear error; sparse caveats default to top).
- `newt-tui`: loading a persona (`--persona` flag or `/persona set`) now parses
  it into a `RoleProfile` stored on the active `Persona`. The prompt overlay is
  injected exactly as before. `/persona show` reports the role, tool allow-list,
  caveat summary, and router policy.
- `--keep-context`: `/persona set <name> --keep-context` swaps the role
  **without** resetting the conversation (the persistent-actor principle). The
  default remains reset-on-swap.
- Shipped templates under `personas/`: `dragon-rider.md` (orchestrator),
  `wing-commander.md` (arbiter), `worker.md` (worker), each exercising the
  front-matter.

## What is explicitly a follow-up (NOT in this slice)

- **Enforcement.** The TUI records the active role's tools/caveats but does
  **not** yet gate tool dispatch on them. There is no agent-bridle registry at
  the dispatch site in the TUI today; this spike deliberately does not invent
  one. Wiring `tools`/`caveats` into an enforcement seam (Landlock / uid-mapped
  namespaces per the agent-bridle design) is the next step.
- **Router policy application.** `model`/`tier` are parsed and surfaced but not
  yet fed into backend selection.
- **Worker / MCP entry points.** `newt-acp-worker` and `newt-mcp-server` should
  read a `RoleProfile` to mint their session authority. Out of scope here.
- **`~/.newt/personas` seeding** of the new templates. The `personas/` dir in
  the repo is a template source; runtime still reads `~/.newt/personas`.

## Naming

The orchestrator role is **dragon-rider** everywhere. The older "foreman" /
"the desk" names are retired and must not be reintroduced.
