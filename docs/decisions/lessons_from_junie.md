# Lessons from Junie (JetBrains): ACP is the IDE-integration substrate

**Status:** Informational (learnings)
**Date:** 2026-06-19
**Related:** #518, #519, #479, #513, `docs/decisions/plain_scroller_tui.md`, `docs/decisions/mesh_integration.md`.

---

## TL;DR

We cloned `JetBrains/junie` for reference. It is **not Junie's source** —
Junie is a closed binary; the public repo is its **distribution/installer**
repo. Like the `pi` clone (see the sibling `lessons_from_pi.md`), junie is a
**telescope — learn from it, don't depend on it.** It surfaced one
load-bearing fact:

> **Junie integrates with IDEs over ACP — the Agent Client Protocol
> (agentclientprotocol.com), the open stdio/JSON-RPC protocol — and newt
> already speaks ACP** (`newt-acp-worker`). The cheapest path to real IDE
> integration is **not** per-IDE plugins; it is **conforming to ACP and
> publishing a registry entry**, exactly as Junie, Zed's ecosystem, and ~36
> other agents already do.

Two consequences, tracked as #518 (newt as an ACP agent IDEs can launch) and
#519 (newt as an ACP *host* that launches heterogeneous pilots).

## What we actually looked at

`~/workspaces/junie` = clone of `JetBrains/junie`. Contents: multi-channel
install scripts (`install.sh` / `.ps1`, plus `-eap` / `-nightly` /
`-experimental`), a `junie.shim.{sh,bat}` that atomically extracts the
downloaded binary, `update-info-*.jsonl` version manifests, `registry-*.json`
files, and installer-shim tests. No agent source code. So the reference value
is in the **integration and distribution model**, not internals we can read.

## Finding 1 — Junie's IDE integration is ACP, and so is ours

Junie ships *itself* as an ACP agent. Its own registry entry launches it as:

```
cmd: ./junie-app/bin/junie   args: ["--acp=true"]   # (per-platform)
```

i.e. the host (IDE, or any ACP client) spawns the binary and speaks ACP over
stdio. This is the same protocol newt already implements:

- `newt-acp-worker/` is an **ACP server over stdio**, invoked via `newt
  worker`. Its README cites agentclientprotocol.com.

**But our implementation is partial and purpose-built.** It was written for
*drake-foreman dispatch*, not for interactive IDE hosting:

- It is hand-rolled — `newt-acp-worker/Cargo.toml` carries a TODO to "depend
  on agent-client-protocol crate (Apache-2.0) for ACP wire layer."
- It implements `initialize`, `new_session`, `set_session_model`, `prompt`
  (+ flat/coder variants) — **not** the full host-facing surface an IDE
  drives (streaming `session/update`, `session/request_permission`,
  `session/cancel`, `fs/read_text_file`, `fs/write_text_file`, capability
  advertisement).
- Its worker contract is hostile to interactive use: the worker only edits
  files (never git), an empty `git diff` after a turn is a **deterministic
  crash** counted against the model scorecard, and `model_id` is mandatory.

Closing that gap is #518.

## Finding 2 — the public ACP agent registry is a launch catalog

The `registry-*.json` files are a **mirror of the public ACP registry**
(`cdn.agentclientprotocol.com/registry/v1`). It is a declarative catalog of
~37 agents — `claude-acp`, `codex-acp`, `gemini`, `cursor`, `goose`,
`opencode`, **`pi-acp`**, `junie`, … — each describing **how to launch it**:

```jsonc
{ "id": "...", "name": "...", "version": "...", "license": "...",
  "icon": "https://cdn.agentclientprotocol.com/registry/v1/latest/<id>.svg",
  "distribution": {
    // one or more of (e.g. codex-acp ships both binary and npx):
    "binary": { "<os-arch>": { "archive": "<url>", "cmd": "...", "args": [] } },
    "npx":    { "package": "...", "args": [], "env": {} },
    "uvx":    { /* python */ }
  } }
```

Distribution modes seen across the registry: `npx` (most), `binary`
(per-platform `{archive, cmd, args}`), `uvx` (Python). Multi-channel
registries (`release` / `eap` / `nightly` / `experimental`) mirror the
installer's staged-rollout channels.

**newt is not in this registry. `pi` is (`pi-acp`).** That is the gap, and
the opportunity: one well-formed entry makes newt launchable from every ACP
host for free.

## What this means for newt — two sides of the same protocol

| Side | What | newt seam | Issue |
|------|------|-----------|-------|
| **Consumer** (newt as agent) | newt is launched by an IDE/host over ACP | `newt-acp-worker` (server side, partial) + a registry entry | #518 |
| **Host** (newt launches others) | a foreman launches heterogeneous ACP pilots from a registry-shaped roster | ACP **client** (not yet written) wired into `CrewRunner` / `compose_roster` (#479) | #519 |

The host side is the more interesting one for us: the registry's
`{binary|npx|uvx}` launch schema **is** a "roster of launchable pilots." It
maps the agent-tier spectrum (`plain_scroller_tui.md`) onto an off-the-shelf
interop format — a foreman could drive `claude-acp`, `codex-acp`, `gemini`,
`pi-acp`, and newt over one wire, selecting by proven competence rather than
provider. We only have the ACP *server* side today; #519 needs the *client*.

## What we are deliberately NOT taking

- **Not adopting Junie** or depending on its binary — it is closed and
  vendor-bound (JetBrains AI Service ToS, JetBrains-account auth). Reference
  only, per the same posture as the `pi` clone.
- **Not building per-IDE plugins.** ACP conformance + a registry entry is the
  whole integration; bespoke IntelliJ/VS Code plugins are the path we are
  avoiding.
- **Not running third-party ACP agents unconfined.** Any host-side launcher
  (#519) must contain pilots we don't audit via agent-bridle caveats / the
  OCAP model — that is a load-bearing open question there, not a freebie.

## Secondary lesson — distribution / self-update UX

Independent of ACP: Junie's **multi-channel installer + self-updating shim +
`update-info.jsonl` manifest** model is a clean reference for newt's own
release/self-update story (newt ships via crates.io / `install.sh` today).
Lower priority; noted for if/when we want channels and in-place updates. The
registry entry in #518 will need per-platform release archives regardless,
which overlaps with this.

## Open questions (carried into the issues)

- Does the foreman worker contract (no-git, empty-diff-crash, mandatory
  `model_id`) require a *separate* interactive ACP mode? (#518)
- How do ACP permission prompts (`session/request_permission`) map onto our
  caveats / agent-bridle leash? (#518)
- Where does a host-side launcher live — newt-agent (client-side ACP) or the
  drake/wyvern control plane? newt is amphibious; the foreman may be the real
  owner. (#519)
- Does ACP carry enough across heterogeneous agents for our scorecard
  (`model_id`, diff capture, deterministic-crash signals)? (#519)

## Provenance / reproduce

- Reference clone: `~/workspaces/junie` (`git@github.com:JetBrains/junie.git`).
- Registry: `registry-*.json` in that repo; canonical at
  `cdn.agentclientprotocol.com/registry/v1`. Spec: https://agentclientprotocol.com
- newt side verified against `newt-acp-worker/` and `newt worker` in
  `newt-cli` on `main`.
