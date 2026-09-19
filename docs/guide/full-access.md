# `--full-access`, and why confinement is the default

By default Newt runs under the `workspace_dev` permission preset. The model
can read and write inside the workspace and run a conservative set of dev
tools; anything else — a path outside the workspace, a network host, another
executable — stops and asks the operator. The grant is enforced by
[`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle),
in the kernel where the OS allows.

`--full-access` lifts that for one run. The workspace fence, the network
leash, and the exec allowlist are all removed; `write_file` still asks. It is
the `full_access` preset applied to this invocation only.

## Why have it at all

A confined agent is only as useful as its grant, and a new task's grant is
not known in advance. Some work needs authority the preset cannot express
yet, and prompting for every step of it is not a real alternative.

So `--full-access` is a way to **discover** a grant, not a place to stay:

1. Run the task with `--full-access`. Newt arms a flight recorder and logs
   the authority each unconfined action used — what a leash would have gated
   on.
2. `newt ocap propose` turns that capture into candidate approvals. Low-danger
   targets are proposed; high-danger ones (interpreters, broad filesystem
   roots) never are.
3. Review the candidates, `--save` them, and bless them with
   `newt doctor --sign-ocap`. Unsigned candidates grant nothing.
4. Run the task again confined. The signed approvals now cover it.

## What keeps it from leaking

- **Per run, by the operator.** There is no config key for it beyond the
  preset itself, so the override cannot silently persist. The model and the
  repository cannot select it.
- **Frozen at startup.** Authority is resolved once when Newt starts;
  `NEWT_FULL_ACCESS` set later cannot widen a running session.
- **Not inherited.** Child processes have Newt's authority switches and
  secrets stripped from their environment.
- **Not the same as `--yolo`.** `--yolo` (`--disable-ocap`) changes *how*
  commands run — the host shell instead of the confined one — and keeps the
  preset's limits. `--full-access` changes *what* is allowed. Both together
  give an unrestricted host shell.

`newt --help` is the authority on the flags; this page explains the intent.
