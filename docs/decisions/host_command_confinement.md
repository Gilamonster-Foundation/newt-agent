# Decision: confine the host command suite at the fence — don't reimplement GNU; allow/attest/deny rides on top

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-22)
**Date:** 2026-06-22
**Related:** `docs/decisions/ocap_confinement_model.md` (confined execution vs.
paths vs. delegated; the honest built-vs-target accounting this depends on),
`docs/design/transparent_command_layer.md` (parse·route·govern; this is its
"how do we give the agent CLI tools" answer), `docs/decisions/structural_parsing_over_regex.md`,
`docs/decisions/agentic_object_capability_security.md`, agent-bridle ADR 0001
(the three enforcement layers L1/L2/L3) + ADR 0002 (the hard invariants, esp. I9
`sandbox_kind` honesty, I10 Landlock, I13 amplify-needs-attest), the step-up /
passkey work (agent-bridle `step_up` `Gate`; newt-core `crew_attest` #479; BOOT
#472), reubeno/brush#1184 + agent-bridle#20 (the brush `CommandInterceptor`).

---

## TL;DR

We are **not** reimplementing the GNU/coreutils suite as confined tools — that is
thousands of tools, per-OS, drifting, never finished, and it buys nothing the
fence doesn't already buy. Instead:

1. **Run the host's commands inside the OS fence.** The fence bounds *any* binary
   regardless of who wrote it, so `grep`/`find`/`git`/`sort` run safely *unmodified*.
2. **Reimplement only the handful where rewriting buys what the fence can't** —
   egress tools (as delegated services) and interpreters (excluded → confined-exec).
   A dozen, not a thousand. **We will NOT rewrite the rest, on purpose.**
3. **The fence is the boundary — and it is not pinned to brush.** It can be the
   brush `CommandInterceptor`, our own `pre_exec` Landlock/seccomp/netns, an
   off-the-shelf `bubblewrap`/`nsjail`, or heavier isolation for workers.
4. **Per-command allow/deny lists are *policy*, never the boundary** (a command's
   identity ≠ its authority). The right policy vocabulary is the step-up decision
   surface — **`{allow, attest, deny}` × presence strength** — riding on the fence.

This doc exists so we **stop relitigating it.**

## The question

agent-bridle's confined shell is a stub (brush#1184 pending), and the OS sandbox
is unbuilt (`verify_b1()` → `Absent`). Faced with that, two tempting wrong turns:
(a) **rewrite every CLI tool** as a confined Rust implementation, or (b)
**allow/deny/prompt-classify every binary** in `/bin:/usr/bin:/usr/local/bin`.
This records why we do **neither as the boundary**, and what we do instead.

## Decision 1 — run the host suite inside the fence; don't reimplement GNU

The fence — the **target shape** (UNBUILT today; see Decision 3 and
`ocap_confinement_model.md`) — is what gives this decision its force: a workspace-
rooted Landlock fs scope + seccomp + default-deny netns, widened only by explicit
`--read`/`--write`/egress grants, bounds **what any spawned process can touch,
regardless of args or who wrote the binary.** A
host `find / -exec …`, `cat /etc/shadow`, or `curl evil` is denied by the
*kernel*, not by our knowledge of the tool. So reimplementing `grep`/`find`/`cat`
in Rust buys **nothing** the fence doesn't already give — and costs a
thousands-of-tools, per-distro, per-OS, perpetually-incomplete maintenance sink.
**We run the host suite, fenced.**

## Decision 2 — reimplement only the egress / escape-hatch handful (explicitly NOT all)

Rewriting a tool is justified **only** where it buys something the fence can't:

- **Egress tools** (`curl`, `wget`) → a **delegated service** (forge-fetch-style:
  the harness holds the credential and the host allow-list; the agent holds a
  bounded request channel — see the transparent-command-layer doc). The win is
  removing the escape hatch and making egress a capability, not a binary.
- **General-purpose interpreters** (`python`, `bash`, `node`, …) → **excluded**
  from the tool surface entirely (their authority isn't parseable); routed to the
  confined-exec path / `--venv` grant.

Everything else — `find grep cat ls sort sed awk git tar diff …` — we **do not
rewrite**. The fence already bounds them, and a reimplementation would be strictly
more work for strictly no security gain. *This is a deliberate non-goal: if a PR
proposes reimplementing a coreutil "for safety," it is rejected — fence it
instead.*

## Decision 3 — the fence is the boundary, and it is not pinned to brush

brush's unique value is an **in-process, capability-aware *shell*** (pipes, globs,
redirects) plus a single spawn funnel for the caveat check. But the **OS fence —
the actual boundary — is separable from brush**, and a *shell* is itself a
launcher/interpreter (the ultimate launderer). So we do **not** put the security
guarantee on brush's critical path. Enforcement options, by preference:

- **(A) `pre_exec` Landlock/seccomp/netns (our own).** Apply the restrictions in
  the child between fork and exec (`CommandExt::pre_exec`, `landlock`/`seccompiler`
  crates). Almost certainly what agent-bridle's `LandlockSandbox` already is
  (agent-bridle ADR 0002 I10). **No upstream dependency.** Spawn **structured argv** (no shell metacharacters → no
  injection surface → parseable authority; ties to the structural-parsing ADR).
- **(B) Off-the-shelf:** `bubblewrap` / `nsjail` wrapping any command (unprivileged
  user namespaces + bind mounts + seccomp). Buy-not-build; Linux-only.
- **(C) Real shell inside the box:** `bash -c '<pipeline>'` run *inside* (A)/(B)
  when shell ergonomics are wanted — enforcement is the box, not an in-shell hook,
  so brush#1184 is moot.
- **(D) Heavier isolation for workers:** gVisor / container / microVM
  (per-session/worker, the airship/FlexClone direction) — overkill per-command.
- **(E) Cross-platform** is its own matrix regardless of brush: macOS **Seatbelt**,
  Windows **AppContainer** (`captured-shell-cross-platform.md`).

**brush `CommandInterceptor` is the *optional ergonomic shell layer*, not the only
path to confinement.** Default posture: structured argv, fenced by (A) or (B).

**Honest status (per `ocap_confinement_model.md`):** the OS fence (`b1-os-isolation`)
is **UNBUILT** today (`verify_b1()` → `Absent`; `sandbox_kind = none`), and the
brush shell is a stub. Until a fence above lands, exec is fail-closed (stub) or
unconfined (`--yolo`). This decision sets the *target shape*; it does not claim
the fence runs today.

## Decision 4 — per-command allow/deny lists are policy, not the boundary

Classifying `/bin:/usr/bin:/usr/local/bin` into allow/deny/prompt by **command
name** is unsound as a *boundary*, because **identity ≠ authority**:

- **The same binary spans every tier by args.** `find /ws …` (read) vs `find / -exec
  cp id_rsa /tmp \;` (read-all + exec + exfil). One allow entry grants the worst form.
- **Transitivity collapse (the killer).** You cannot consistently *allow* a launcher
  and *deny* its target: `find -exec`, `xargs`, `env`, `awk system()`, `git -c
  core.pager=…`/hooks, `tar --to-command`, `make`, `nohup`, `timeout`, … each
  spawns the "denied" command. The commands worth allowing are exactly the ones
  that defeat the deny-list.
- **Deny-lists fail open** (allow-by-default-except-listed = ambient authority with
  holes; every newly-installed binary is allowed). OCAP enumerates *goodness*, not
  badness.
- **Name-matching is spoofable** (symlinks, PATH-shadowing, busybox/toybox
  multi-call applets, `c"u"rl`, `$(echo curl)`) — the regex-vs-AST footgun again.
- **Even a perfect allow-list is fenceless** — it controls *which* binary runs,
  never *what it touches*; `cat ~/.ssh/id_rsa` is allowed. (The `--venv` lesson.)

So lists are **surface + intent**, not the wall. We keep the vocabulary, drop
"lists as the boundary."

## Decision 5 — the policy is `{allow, attest, deny}` × presence strength

The right policy layer **on the fence** is the existing **step-up decision
surface**, not an ad-hoc list. It is two-dimensional:

- **Decision:** `{allow, attest, deny}` — agent-bridle's `Gate`/`step_up`
  primitive (agent-bridle#24). newt-core's `crew_attest` (#479) applies it to crew
  dispatch: `crew_authz → {Allow, NeedsAttest(Presence)}` (a held op surfaces
  `NeedsAttest`; `deny` is the Gate-level verdict).
- **× Presence strength:** `{none, presence-prompt, passkey/biometric}` — so an
  `attest` ("prompt") can range from a soft confirm to a **required hardware
  liveness gesture (YubiKey touch / Touch ID)**. This is `Presence::{Prompt,
  Passkey}`, the passkey step-up work. Per I13, *amplifying* authority always sits
  at the strong end (human root via a FIDO2 gesture); attenuating is free.

A policy entry is therefore `(matcher) → {allow | attest(presence) | deny}`,
applied **after** the fence has already bounded the blast radius — so a
mis-approved or laundered command is still contained. The matcher keys on the
parsed op/resource (structural), not a raw command-name string.

**Honest status:** the decision *surface* is built (`crew_attest` #479; the
`Presence` shape exists). **Real passkey/biometric *enforcement* awaits BOOT
(#472)** — today `Presence::Prompt` soft-allows and `Passkey` surfaces
`NeedsAttest` without hardware teeth. Structure, not yet enforcement; marked so
per the honesty rule.

## Consequences / rules

1. **A PR that reimplements a coreutil "for safety" is rejected** — fence it
   instead (Decision 2). Reimplementation is reserved for egress/escape-hatch tools
   as delegated services.
2. **No per-command allow/deny *list* is presented as a security boundary.** Lists
   are surface-reduction; the boundary is the fence; the gate is `{allow, attest,
   deny} × presence` (Decision 4–5).
3. **The fence is decoupled from brush** (Decision 3): the security critical path
   is the OS fence (pre_exec-Landlock / bwrap / per-OS), with brush as optional
   shell ergonomics. Default to **structured argv**, not a shell.
4. **Honesty:** every claim here is true with `verify_b1() == Absent`, the stub
   shell, and `--yolo` on the table. Built is built; target is target.
