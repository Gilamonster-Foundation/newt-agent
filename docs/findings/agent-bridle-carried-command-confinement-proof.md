# Proving That an Allowed Carried Command Cannot Escape Agent Bridle

**Repository:** `Gilamonster-Foundation/agent-bridle`  
**Target:** `BrushShellTool` with `carried-coreutils`  
**Original finding date:** 2026-07-23  
**Status:** Historical pre-remediation proof input, with Agent Bridle 0.7.14
implementation status added 2026-07-24

> [!IMPORTANT]
> The executive decision and security theorem below were written against the
> 2026-07-23 pre-remediation implementation and are preserved as written. Where
> they say “currently” or describe a gap as open, they describe that historical
> baseline — they are not claims about Agent Bridle 0.7.14 and are not a
> current release checklist. The status immediately below records the
> implemented and verified 0.7.14 boundary without rewriting the original proof
> input after the fact. The threat model, proposed architecture, unchecked
> acceptance list, and source notes that stood in the retired sections are in
> git history before the 2026-08-18 condensation.


## Agent Bridle 0.7.14 post-fix status

The 0.7.14 implementation closes the carried-command filesystem gap identified
by this document:

- Brush and carried utilities run behind an authenticated, same-image private
  worker-control channel on Linux and macOS. The worker and carried descendants
  inherit the selected native L3 boundary before untrusted shell evaluation.
- Restricted filesystem axes require kernel enforcement and fail closed on a
  downgrade. Successful envelopes report the achieved sandbox and per-axis
  enforcement instead of presenting the carried path as
  `SandboxKind::None`.
- The public facade exposes
  `Registry::dispatch_with_strength_floor(...)`, so an embedder can bind the
  enforcement strength reviewed for an exact operation to the actual dispatch.
  The ordinary `dispatch(...)` path retains its advisory default.
- The private worker entry point is no longer a public `dispatch_host` binary.
  Same-image authentication, request binding, and platform capability checks
  prevent an unauthenticated caller from manufacturing the worker transition.
- Platforms without that authenticated private-control mechanism fail closed.
  In particular, Windows does not advertise Brush and the facade falls back to
  the structurally restricted `ShellTool`.

Verification completed for the 0.7.14 handoff:

- 127 `agent-bridle-core` tests and 50 shell-tool tests;
- real Brush and carried-coreutils harnesses, including allowed/denied
  filesystem behavior;
- private-control forgery and substitution regressions, PS4 injection,
  enforcement-downgrade, facade, and platform-selection regressions;
- targeted Linux and Windows cross-checks and the macOS Seatbelt test;
- formatting, Clippy with `-D warnings`, packaging, and diff checks.

The remaining boundary is intentionally narrower than “all shell descendants
are mediated by Brush.” An admitted external program can create grandchildren
that do not re-enter Brush’s L2 interceptor. Embedders must therefore require a
kernel strength floor for plans containing external programs; an advisory floor
is suitable only for carried/in-worker-only plans, while Agent Bridle
independently continues to require kernel enforcement for restricted
filesystem axes. This addendum does not claim that every aspirational matrix
item in the historical checklist below was implemented, nor does it expand the
filesystem proof into Linux program-identity or hostname-network confinement.

## Executive decision

An allowed carried command must not be trusted merely because:

1. Brush admitted the command name;
2. the command was compiled into the Agent Bridle binary; or
3. the `before_exec` interceptor observed the re-exec.

Those facts establish **command admission**, not **confinement of the command's internal behavior**.

A carried `cat`, `head`, `cp`, `dd`, or `sha256sum` opens files inside its own process. Those opens do not pass through Brush's `before_open` hook. Therefore the security claim must be established below Brush, at the operating-system boundary.

The required architecture is:

> Run the entire Brush worker and every carried-command descendant inside an inherited, kernel-enforced filesystem sandbox derived from the effective `Caveats`. Refuse the invocation when a restricted filesystem axis cannot be enforced.

For Linux, the immediate implementation target is Landlock. The preferred cross-platform shape is a dedicated sandboxed Brush worker process, launched through the same confinement machinery already used by `ConfinedCommand`.

The proof is deliberately limited to the filesystem axes:

- `fs_read`
- `fs_write`

It does **not** prove complete Linux program-identity confinement for `exec`. Agent Bridle's own ADR 0011 correctly documents the loader/interpreter trampoline that prevents Landlock from making that stronger claim.

---


## Security theorem

Let:

- \( C \) be the effective `Caveats` minted by `Gate::authorize`;
- \( R(C) \) be the set of filesystem read roots granted by `C.fs_read`, plus the explicitly disclosed runtime loader/data roots;
- \( W(C) \) be the set of filesystem write roots granted by `C.fs_write`, plus narrowly defined device sinks such as `/dev/null`;
- \( P \) be the Brush worker and every process descended from it;
- \( op(p, x) \) be a filesystem operation attempted by process \( p \) on object \( x \).

The target claim is:

```text
For every p in P:

  read-like(op)  and x not in R(C)  => the kernel denies op
  write-like(op) and x not in W(C)  => the kernel denies op
```

This includes operations performed:

- directly by Brush;
- by a carried uutils command;
- by a host command admitted by Brush;
- by a child or grandchild spawned by an admitted command;
- through path traversal, symlinks, hard links, `/proc/self/fd`, or shell redirection.

The claim is valid only under the explicit assumptions in the next section.

---

**Status:** condensed 2026-08-18. This was a pre-remediation proof input for
`Gilamonster-Foundation/agent-bridle`, and the gap it describes was remediated
in agent-bridle 0.7.14. The required-architecture, code-change, and test-plan
sections addressed a design that has since shipped, so they were retired; the
post-fix status, the executive decision, and the security theorem are kept
because they are the durable claims. The full analysis is in git history before
this commit.
