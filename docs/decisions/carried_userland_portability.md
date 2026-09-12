# Why the shell is carried, not borrowed

**Status:** Rationale, recorded 2026-09-12 after it turned out to be nowhere in
the repo. The decision itself is old and already implemented — the
`carried-coreutils` and `brush` features are on by default. This records *why*,
because the reason was load-bearing and only lived in the maintainer's head.

**Refs:** [`brush_confined_shell_handshake.md`](brush_confined_shell_handshake.md)
(the crates.io packaging problem, a different question);
[`../findings/agent-bridle-carried-command-confinement-proof.md`](../findings/agent-bridle-carried-command-confinement-proof.md);
[`../design/infinite-context-illusion.md`](../design/infinite-context-illusion.md)
(unrelated, but the same "absent must never be ambiguous" law appears in §4.1
there); issue #2274.

## The decision

**We wanted brush available in every environment.** The confined `run_command`
shell is the real brush OCAP shell, and its coreutils are shims that re-exec
the bridle binary under other names
(`agent-bridle-tool-shell/src/brush_shell.rs:609`). The agent's userland is
therefore *carried*, not borrowed from the host.

## Why: host shells are not the same shell

A borrowed userland makes behaviour a function of the machine:

| host | what you get |
|---|---|
| Linux | GNU coreutils, `sh` is usually dash or bash |
| macOS | BSD coreutils — different flags, different output |
| Windows | no `sh` at all |

An agent whose tool results change shape by platform cannot have its behaviour
reasoned about, tested once, or reproduced from a trace. Carrying the userland
makes the shell a property of **newt**, not of the box, so a recorded run means
the same thing everywhere.

## The evidence, from the day this was written

PR #2277 added the refusal that distinguishes "not carried" from "denied" from
"not on this host". Its test needed a binary known to exist on the host, and
picked `sh`:

```rust
const PRESENT: &str = "sh";
```

Windows CI failed —
`not_carried_but_present_on_host_names_the_grant_to_ask_for` — because Windows
runners have no `sh`. **The production logic was correct; the fixture made a
platform assumption.** A test reached outside the carried world to name a host
binary and was immediately bitten in exactly the way carrying brush exists to
prevent.

That is the argument in one test case, and it generalises:

> **Never hardcode a host binary name, including in tests.** Any name is a
> platform assumption. Where a test needs a file that certainly exists on the
> host, `std::env::current_exe()` is the portable answer — the test binary
> itself, with an absolute path.

Fixing such a failure with a `#[cfg(windows)]` constant moves the assumption
rather than removing it, and costs the coverage on the platform where host
assumptions actually break.

## The cost, stated honestly

Carrying the userland buys **consistency** and charges **reachability**. A tool
that is not carried does not exist from inside the fence — not `cargo`, not
`just`, not `gh`, not `grep`. This is why the confined coding profile was
unusable for roughly six months, and the failure was hard to see because it
surfaced as `command not found`, which is indistinguishable from a broken
machine rather than from a policy decision (#2274, #2273).

Two things follow, and both are in flight:

1. **Absence must be named** (#2274 PR A / #2277). Three states hide behind one
   exit code, and the right next move differs in each: denied by a grant, not
   carried but present on the host, or not on the host at all. The refusal says
   which.
2. **Reachability is restored without giving up the fence** (#2274 PR B). Named
   host binaries, granted by absolute path, executed as children under the
   Landlock/seccomp fence that is already enforced. Carried for consistency,
   granted for reach.

Neither half is sufficient alone. The carried userland without named host
binaries is the unusable profile; host binaries without the carried userland is
the platform-dependent behaviour this decision exists to avoid.

## What this does not say

It does not say the host lane (`--unsafe-host-exec`) is illegitimate. It says
the host lane must not be the *only* way to reach a host binary, because a
choice between a curated userland and the whole machine is not a choice an
operator should have to make for one missing compiler.
