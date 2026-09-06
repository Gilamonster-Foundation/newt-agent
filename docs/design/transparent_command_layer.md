# Design: the transparent command layer (parse · route · never grant ambient authority)

**Status:** Design / proposed (Shawn Hartsock, 2026-06-21)
<!-- INERT-CODE-RATCHET: S04 DOCUMENT: three documents describe nonexistent forge resolver code as shipped or current. -->
**Related:** `docs/decisions/structural_parsing_over_regex.md` (the AST rule this
relies on), `docs/decisions/host_command_confinement.md` (the decision: fence the
host suite, don't reimplement GNU, allow/attest/deny × presence on top),
`docs/decisions/ocap_confinement_model.md` (what "confined" honestly
buys — and the `--yolo` caveat),
`docs/decisions/agentic_object_capability_security.md` (the OCAP
leash this must not break), `docs/design/captured-shell-ocap.md`,
`docs/security/ocap-deviations.md`, brush `feat/shell-extensions-cap-hook` (the
tool-call hook), issue #552 (hide/route embedded git), and the first shipped
slice — `newt-tui/src/forge_context.rs` + `newt-core/src/forge_resolvers.rs`
(the forge-URL input resolver).

---

## TL;DR

newt can recognize a family of well-known invocations — forge URLs, and CLI
calls **whose intent is parseable** — `gh`, `glab`, `git`, `curl`, `wget`,
`find`, `grep`, … — **parse them structurally (AST)**, and **handle them
transparently** so the model isn't tripped up by which tools are embedded vs.
shelled-out (the #552 confusion). The non-negotiable constraint: this convenience
layer must **not** reintroduce ambient authority. It is a **parser + capability
router**, never an authority *source*.

**Inclusion criterion:** a command belongs in this layer *iff its authority is
determinable from a structural parse of the invocation* (which host it reaches,
which path it reads, which operation it performs) — so the layer can bound it.
General-purpose interpreters (`python`, `bash`, `node`, …) **fail this test and
are excluded**: no parse of `python script.py` reveals what the script will do.
They are not "in the layer but confined" — they are not candidates at all, and
keep going through the existing confined-exec capability (agent-bridle), governed
there. See *Excluded* below.

## The opportunity

Two observations converge:

- The **forge-URL resolver** (shipped first) detects `github.com/…/issues/N` in a
  message, parses it with the `url` crate, and fetches it out-of-band via
  `gh`/`glab` (REST fallback). That is *pattern → structural parse →
  substitute/route*.
- **#552**: the model, after using newt's embedded git, later tries `git status`
  in a shell that isn't there and gets confused. The fix is the *same shape* —
  recognize `git …`, route it to the embedded tool, transparently.

Generalized: a **structural-parse front-end** (AST for URLs *and* CLI argv) feeds
a **handler registry**; handlers fire at **two hooks** — user input, and the
model's tool calls (the brush `shell-extensions-cap-hook`). `curl`, `wget`,
`git`, `gh`, `glab`, `find`, `grep`, … all become handlers on one primitive.

## The hazard — why this can break OCAP

newt is an object-capability system: the agent may do only what its capabilities
permit. A transparent layer that simply *runs* whatever CLI the model emits is
**ambient authority** — the exact confused-deputy failure OCAP exists to prevent.
`python` is the limit case: ambient `python` is arbitrary code execution, i.e.
unbounded authority. "Abstracting the functionality" must not smuggle that back
in — which is exactly why interpreters are *excluded* from the layer rather than
"handled carefully" (see Principle §4): the safe move is to not claim what you
cannot bound.

## The principle

**The transparent layer parses and routes; OCAP is preserved underneath it.**

### 1. Authority owner decides the rule

- **User-initiated** (the human typed the URL / `curl X`): the *user's* authority.
  The user is root; the harness is their deputy. Safe to handle transparently.
- **Agent-initiated** (the model emitted `curl X` / `python …` as a tool call):
  the *agent's* authority. It MUST pass the leash. Here "transparent" means a
  uniform *interface*, never ungoverned power.

### 2. The AST is the enabler of the check

You can only authorize what you can truly see. Parse
`curl https://evil.internal/x` → host `evil.internal` → check the egress caveat →
deny. A regex guess is a parser differential, and a parser differential at an
authority boundary is an authority bypass. (See the structural-parsing ADR.)

### 3. Commands tier by the authority they request

The layer handles exactly two tiers — both with authority you can read off the
parse:

| Tier | Examples | Routing | Governance |
|------|----------|---------|------------|
| **Read-only** | `git status/diff/log`, `grep`, `find`, `cat`, `ls`, `gh/glab … view` | embedded / confined read impl | read caveat; **fenced to the workspace at the execution boundary** (§5) — not by parsing each command's path flags |
| **Egress** | `curl`, `wget`, `git push/fetch`, `gh/glab … create` | out-of-band fetch / embedded writer | egress caveat: host allow-list, write scope (the forge resolver's host-allowlist + HTTPS-only + no-redirect *is* this) |

### 4. Excluded: general-purpose interpreters

`python`, `bash`, `sh`, `node`, `perl`, `ruby`, … are **not in this layer.** They
fail the inclusion criterion: their authority is *not* determinable from the
invocation — `python script.py` can do anything the process can, so there is
nothing to parse, tier, or bound. Pretending to "route them to a confined
executor" would smuggle the unparseable case back into a layer whose whole value
is parseability.

So arbitrary execution stays where it already belongs: the **confined-exec
capability** (agent-bridle / the captured shell), governed by an explicit caveat
there, default-deny. The transparent layer neither recognizes nor handles
interpreters — it simply doesn't claim them. This keeps the layer's guarantee
honest: *everything it touches, it can bound.*

For Python specifically, a governed path already exists: **`newt --venv <path>`**
(and `[tui.permissions] extra_exec`) — an explicit human grant that injects a
chosen venv, prepends its `bin/` to `PATH` in the confined shell, and permits
exec of that venv's executables. **But be precise about what that buys** (see
`docs/decisions/ocap_confinement_model.md`): granting exec to *everything in a
directory* is not a tight capability — a venv `bin/` is mutable and plant-able,
so `--venv`'s safety is **the fence, not the allow-list**. It is *region-fenced
ambient exec* — the coarse floor — not a bounded capability over which code runs.
The genuinely tight surface is the curated, escape-hatch-free toolbox exposed as
**delegated capability services** (above), which the agent invokes rather than
holds. Excluding `python` from the layer costs nothing: it points at the floor
(`--venv`, fenced) for the unwrapped case and at delegation for everything we do
wrap — never at ambient authority.

### 5. File access is fenced where it executes, not by parsing arguments

"Fence every file-accessing command" — `find`, `grep`, `cat`, `ls`, `git`,
`curl -o`, … — is **not** done by teaching the layer each command's
path-argument grammar. That is fragile: a missed flag, a path named in a config
file, a symlink, or a `$(…)` would slip the fence. The right place is the
**execution boundary**. What that *buys* depends on what is built — and we are
honest about the gap (see `docs/decisions/ocap_confinement_model.md`):

- **The target** is an OS sandbox — `b1-os-isolation` (`newt-core::ocap`:
  Landlock/Seatbelt/AppContainer + seccomp/netns), widened only by explicit
  `--read`/`--write` grants — which *would* deny a spawned process any access
  outside the fence regardless of how it names its paths. **This is UNBUILT
  today:** `verify_b1()` returns `Absent` (`sandbox_kind = none`).
- **What's built today** is the in-process monitor: `lock_fs_to_workspace`
  (`newt-core::caveats`) enforces the fs scope at `tui_permits_path` for newt's
  **native** tools (`read_file`/`write_file`/`list`) — *not* for a spawned
  subprocess. So a file-accessing **command** run via the confined shell is, in
  this build, fail-closed (stubbed, "unavailable"); under `--yolo` it runs on the
  host shell with **no fs fence over the subprocess at all**.

So "fence all file-accessing commands" is the **design target** (it arrives with
the OS sandbox), not a present guarantee over subprocesses. Until `b1` lands:
native-tool fs is in-process-fenced; a routed command is confined-shell-stubbed
or `--yolo`-unfenced. The honest doctrine: *fence at the execution boundary once
the sandbox exists, prefer delegated services meanwhile, and never claim a
containment the verifier marks `Absent`.*

Per-command path parsing (the earlier `find <path>` → cwd check) is kept as
**defense-in-depth / fast-fail** — a clear "that path is outside the workspace"
error — useful both before the OS fence exists and after; never the boundary by
itself.

### The one-line rule

> The transparent command layer is a **structural parser + capability router**,
> not an authority source. It claims a command only when the parse reveals (and
> can bound) its authority; agent-initiated invocations land on a
> capability-checked implementation; user-initiated ones carry the user's own
> authority. Anything unparseable — general-purpose interpreters above all — is
> **not claimed**, and goes to the confined-exec path instead.

## Architecture

```
                 ┌──────────────── structural front-end (AST) ────────────────┐
 user input ───▶ │  url::Url for links · argv/command parse for CLI calls       │
 model tool ───▶ │  (NEVER regex at the boundary — see the ADR)                 │
                 └───────────────────────────┬─────────────────────────────────┘
                                             ▼
                        ┌──────────── handler registry ────────────┐
                        │  trigger (host+path | command+arg shape)  │
                        │  tier (read | egress | exec)              │
                        │  action (fetch | embedded tool, host/path-fenced)│
                        └───────────────────┬───────────────────────┘
              user-authority ▼                         ▼ agent-authority
        transparent (deputy of the user)        leashed (caveat-checked) — deny by default
```

- **Front-end:** the `url` crate for links; a real argv/command parser for CLI
  calls. No regex at the boundary.
- **Registry:** built-ins + `~/.newt/issues.toml`-style config (override by name,
  add forges/commands). Each entry: trigger, tier, action.
- **Hooks:** input (this repo) and tool-call/shell (brush). Same registry.

## Phased delivery (forced by the repo boundary)

The tool-call/shell hook lives in **brush** (`feat/shell-extensions-cap-hook`), a
separate repo — so the command-interception handlers cannot land in a newt-agent
PR. Delivery is therefore phased, each phase reusing the shared primitive:

1. **Foundation + forge-URL input resolver** *(this PR)* — the AST front-end +
   registry + the user-input hook for `github`/`gitlab` links. Transparent,
   structural, SSRF-hardened, config-driven, default-on with an off switch.
2. **Tool-call hook + `git` routing (#552)** — brush `shell-extensions-cap-hook`
   consumes the registry; `git` routes to the embedded tool (read caveat).
3. **`curl`/`wget`** — egress handlers reusing the URL AST + the egress caveat.
4. **`gh`/`glab` command-form + the leashed `forge_fetch` tool** — agent-initiated
   forge access, OCAP-governed.
5. **`find`/`grep`/… read-only** — confined read impls, **path-fenced to the
   workspace** (`find <path>` refused unless it resolves under cwd).

**Not a phase: interpreters.** `python`/`bash`/`node`/… are excluded from the
layer entirely (Principle §4) — they remain on the existing confined-exec
capability (agent-bridle), default-deny. There is no "exec phase" here, by design.

## Consequences

- Extending the command list is safe *only* by classifying the new command into a
  tier and giving agent-initiated use a caveat. A handler that runs an
  agent-emitted command ambiently is a review blocker — it breaks OCAP.
- The forge-URL resolver is the reference implementation of tiers 1–2 of the
  pattern (structural parse, host allow-list, egress discipline).
- This doc is the standing answer to "can we just transparently run `<command>`?"
  If the agent initiated it and it isn't capability-checked, the answer is no.
