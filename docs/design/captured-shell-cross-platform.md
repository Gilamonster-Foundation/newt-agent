# Captured-shell OCAP — the cross-platform brush vision

**Status:** Design (2026-06-16). Companion to
[`captured-shell-ocap.md`](./captured-shell-ocap.md), which it **amends**.
No code; this grounds the long-term vision the original doc left implicit.

## Why this doc exists

`captured-shell-ocap.md` is sound on the *authority algebra* and the
*interrogated-shell model*, but it quietly assumed two things it never argued:

1. **That `brush` is the interception layer** — "newt's confined shell is
   `brush`, so interrogation can be interpreter-level" (its §2). In reality
   `brush` is **not wired**: agent-bridle is pinned to a `feat/stub-shell`
   branch whose `shell` tool fails closed without the brush `CommandInterceptor`
   patch (reubeno/brush#1184, agent-bridle#20), and the only working exec path
   today is an unconfined host `bash -c` with `sandbox_kind none`. The
   interpreter-gate is **aspirational**, not built.
2. **That B1 is Linux primitives.** The original §4/§"three MUSTs" name
   "uid-namespace + Landlock + seccomp + netns + egress proxy" as *the* B1.
   Those are **Linux-only**. newt is explicitly tri-platform — macOS (the
   author's daily driver, Apple Silicon), Windows, and Linux — and the macOS
   and Windows confinement substrates are completely different APIs. As written,
   B1 is unbuildable and untestable on two of the three target platforms, which
   silently means "the invariants are off" exactly where the original doc warned
   they must not be.

This doc fixes both: it makes **brush the *portable* interpreter-gate** the
long-term vision actually depends on, and it specifies B1 **per operating
system** — plus a deliberate **native-shell carve-out** (`bash`/`zsh`/
`powershell`) for compatibility and platform-specific capability.

## The two-layer model (defense in depth)

Confinement is **two independent layers**; neither is "the boundary" alone
(the original doc's load-bearing reframe — redaction is not the boundary —
generalizes here):

- **Layer A — the interpreter-gate (prevention).** A shell newt controls turns
  every `exec` / `open` / `connect` into a **structured event checked *before*
  it commits**. This is `brush` + `CommandInterceptor`. It is *portable by
  construction*: the gate is newt's own code, identical on every OS, so it gives
  the same `before_exec`/`before_open`/`before_connect` semantics where the OS
  primitives diverge wildly. It is **prevention, not detection** — and it is the
  only layer that can reason about *intent* (`cat $TOK | base64` is two gated
  spawns, not an opaque byte stream).
- **Layer B — the OS-sandbox substrate (containment backstop).** The kernel/OS
  confinement that holds **even if the interpreter-gate is wrong or bypassed**
  (a brush parser-differential, a `LD_PRELOAD`, a native binary that doesn't go
  through brush at all). This is B1, and it is **per-OS**.

The original doc collapsed these into "brush + Linux B1." Keeping them distinct
is what makes the vision real cross-platform: **Layer A is uniform (brush);
Layer B is platform-specific (the matrix below).** A live credential may be
seeded only when *both* layers are fail-closed on the host's platform.

## brush as the cross-platform interpreter-gate (Layer A)

`brush` is a Rust shell newt embeds and controls. The reason it is the
*right* Layer A — and why a native shell cannot be — is **uniform structured
interception**:

- One interception model (`CommandInterceptor`: `before_exec`, `before_open`,
  `before_connect`) compiled into newt on all three OSes. The admission gate and
  disclosure gate from `captured-shell-ocap.md` §2 hang off these hooks
  unchanged per platform.
- It is the wyvern invariant already: the plain-scroller decision records that
  wyvern-agent is "**locked into the embedded brush** — no escape hatch." That
  lock only means anything if brush is the gate on *every* platform a wyvern
  flies on.
- Cost: once brush executes effects, **brush enters the TCB** (parser-
  differential between the interceptor's view and the executor's effects) — the
  original doc's residual risk #"brush enters the TCB" stands. Layer B is the
  backstop precisely for this.

**Status to ship Layer A:** land the brush `CommandInterceptor` upstream, move
agent-bridle off `stub-shell` onto a pinned audited brush revision, and route
`run_command` through it. Until then there is **no Layer A on any platform** —
which is the honest current state.

## The B1 substrate matrix (Layer B, per-OS)

| Capability | Linux | macOS | Windows |
|---|---|---|---|
| **Filesystem confinement** | Landlock LSM (+ mount/user namespaces) | Seatbelt / `sandbox_init` profiles (`sandbox-exec`); App Sandbox entitlements if bundled | AppContainer profile + capability SIDs; restricted/lowbox token |
| **Syscall / API restriction** | seccomp-bpf | Seatbelt operation filters (no raw syscall filter; profile-scoped) | Job Object limits + restricted token + (optionally) a syscall/AppContainer policy |
| **Network egress (the proxy is the boundary)** | network namespace + the *only* route is the egress proxy (DNS included) | `sandbox-exec` `(deny network*)` + a local egress proxy as the sole allowed peer; Network Extension content-filter if bundled | WFP (Windows Filtering Platform) / AppContainer network capability stripped except loopback to the egress proxy |
| **Process isolation** | namespaces (pid/ipc/uts) | per-process Seatbelt; no namespace equiv | Job Object + separate desktop/station optional |
| **Maturity / caveat** | Mature, well-trodden | `sandbox_init` API is **deprecated** (but functional and still used widely); App Sandbox is the supported-but-heavier path (requires bundling/entitlements) | AppContainer is mature for store-style apps; wiring it for an embedded shell is the most bespoke of the three |

**Binding rule (amends `captured-shell-ocap.md` §5.1):** "do not seed a live
token until B1 is a fail-closed precondition" means **the host platform's B1**.
B1 is not one artifact; it is three, and the credential-seeding gate must
runtime-verify the *local* platform's substrate is active and fail-closed — not
assume Linux. A platform without a built, verified B1 is a platform that **must
refuse** credential seeding, not silently run with Layer B absent.

## The native-shell carve-out (`bash` / `zsh` / `powershell`)

There is real, legitimate pressure to run the **native** platform shell rather
than brush, for two reasons:

1. **Compatibility.** Users have rc files, aliases, functions, and existing
   scripts. Real tools assume a real shell — `pa login`'s SAML browser dance,
   PowerShell cmdlets and providers, platform package managers, completion
   engines. brush is a reimplementation and *will* lag native behavior in
   corners.
2. **Platform-specific capability.** Some work is only expressible in the native
   interpreter (PowerShell's object pipeline and WMI/CIM access on Windows;
   zsh/bash idioms and platform CLIs on macOS/Linux). brush deliberately won't
   reach all of it.

The carve-out, and its hard boundary:

- **A native shell is Layer-A-blind.** `bash -c "…"` is an **opaque byte
  stream** — no `before_exec`/`before_open`/`before_connect`, no admission gate,
  no disclosure gate. Choosing a native shell **forfeits Layer A entirely.**
- **Therefore a native shell is permitted only under a *stricter, fail-closed*
  Layer B.** With no interpreter-gate, the OS sandbox is the *only* barrier, so
  it must be tightened (no interpreter-spawn beyond the named one, egress proxy
  mandatory, fs fence minimal) — never the relaxed "trust the gate" posture.
- **It is gated and labeled, never the default for model-driven exec.** The
  agent's confined `run_command` defaults to brush (Layer A + B). A
  native-shell mode is an explicit, audited opt-in for compatibility work,
  surfaced like `--disable-ocap` is today (a human-in-the-loop affordance, not
  an unattended one).
- **The human `!` bang-escape is the already-shipped native-shell path — and is
  *correct*.** It runs `$SHELL -c` / `%COMSPEC% /C` with the human's own
  authority and **no leash**, because the model can never invoke it (see
  `docs/decisions/plain_scroller_tui.md`). That is the trusted-human carve-out:
  native shell, full compat, no confinement, because the actor is the human.
  Keep it **distinct** from the *agent's* confined exec — the carve-out above is
  about letting the **model's** confined path fall back to a native shell under
  stricter B1, which is a different and much narrower decision.

So the design supports both intents without confusing them: **brush is the
default gate (Layer A) everywhere; native shells are a compat carve-out that
trades Layer A for stricter Layer B; and the human bang-escape stays the
unconfined human path it already is.**

## What this changes about `captured-shell-ocap.md`

- §2 "newt's confined shell is brush" → true as a *goal*; brush is **not wired**
  yet (stub-shell). Add that caveat.
- §4 finding #1 / the "three MUSTs" → B1 is **per-OS** (the matrix above), not
  "landlock + seccomp + netns." The MUSTs are correct *as a shape*; they must be
  instantiated three times.
- §5.1 "do not seed a live token until B1" → "…until the **host platform's** B1
  is built and runtime-verified fail-closed."
- New residual risk: a native-shell carve-out forfeits Layer A; accept it only
  under stricter Layer B, and never conflate it with the human bang-escape.

## Open questions

- macOS: `sandbox_init`/`sandbox-exec` (deprecated, lighter, no bundling) vs App
  Sandbox entitlements (supported, heavier, requires a signed bundle) — which is
  the B1 newt ships, given newt is a plain binary, not an `.app`?
- Windows: is AppContainer worth the bespoke wiring for an embedded shell, or is
  a restricted-token + Job Object + WFP gatekeeper a sufficient first B1?
- Egress proxy: one portable proxy implementation consumed by all three netns/
  sandbox/WFP front-ends, or per-OS proxies?
- brush compat coverage: which native-shell features are common enough that
  their absence forces the carve-out, and can brush close that gap instead?
