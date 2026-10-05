# Full access inside a workspace

To let Newt work freely inside one directory, start it with:

```sh
newt --workspace-access code /path/to/project
```

Commands can create, change, rename, and delete files and directories beneath
that workspace, including recursive cleanup, without approving each in-scope
filesystem operation.
The native filesystem fence still protects outside targets, including `..`
and symlink escapes. Explicit `--read` and `--write` grants can open additional
paths. Network access follows the separately configured network permissions;
this option grants no network access by itself.

To save a policy for a particular workspace, open `/settings workspaces` (or
`/workspace`) in a protected interactive session. The editor offers a resolved
preview and explicit confirmation, then applies the saved profile on the next
real Newt process launch. `/restart` does not reload authority. See
[saved workspace access](workspace-settings.md) for directory management,
default-workspace selection, independent approvals, and protection requirements.
To persist this filesystem and executable preset, use an operator-owned, trusted
config such as `~/.newt/config.toml` (or `$NEWT_CONFIG_DIR/config.toml`):

```toml
[tui.permissions]
preset = "workspace_full_access"
```

Repository permission settings in an ambient `newt.toml` or project
`.newt/config.toml` are not trusted. To select a particular operator config,
set `NEWT_CONFIG` to its existing absolute path. This also avoids an ambient
`newt.toml` taking precedence over the user config.

The startup choice uses Newt's signed capability path and does not change
authority in an already running session. `--workspace-access` conflicts with
`--full-access` and `--yolo`; inherited global-access and bypass switches do not
override the requested directory fence. The persistent preset alone does **not**
clear inherited `NEWT_FULL_ACCESS` or `NEWT_DISABLE_OCAP`; use the explicit
`--workspace-access` flag to select that confined startup behavior. An active
posture or operating mode may still narrow the session's authority.

A saved workspace profile is different from that general config preset: its
startup path selects confinement even when global-access switches were inherited.
Explicit conflicting `--full-access` or `--yolo` flags are refused. Explicit
`--workspace-access` keeps the saved directories while selecting workspace full
access for that one run. Removing the saved profile restores configured fallback
behavior and leaves independent saved approvals intact.
On macOS, the current Bridle backend cannot enforce restricted network grants,
including the default empty `net` list. Confined commands require the operator
to explicitly allow unrestricted networking. For that choice, include this in
the same trusted config:

```toml
[tui.permissions]
preset = "workspace_full_access"
net = ["*"]
```

The directory fence remains active. Destination filtering and the proposed
traffic-monitor TUI are still [planned work](../design/network-console.md);
this setting grants network access and does not provide those controls.

Build, test, and formatting commands may ask for a calibrated Build grant to
read toolchain/package caches beyond the workspace and use managed scratch.
Allowing Build adds no network authority. Supported native `git commit` commands
retain harness attribution/signing and default-branch protection: commit on a
feature branch. Merge/rebase families and native Windows commit transport remain
outside the current [native commit adapter's scope](../design/agent-toolchain.md#native-commit-adapter-scoped-implementation).

`/help permissions` describes the startup choice. `/permissions` and `/allow`
review session decisions; they do not edit the preset. Terminal permission
prompts can grant access for the session and offer permanent grants for eligible
filesystem, executable, and network requests.
The workspace editor neither revokes these approvals nor changes active jobs.

## Global full access

By default Newt runs under the `workspace_dev` permission preset. The model
can read and write inside the workspace and run a conservative set of dev
tools; anything else — a path outside the workspace, a network host, another
executable — stops and asks the operator. The grant is enforced by
[`agent-bridle`](https://github.com/Gilamonster-Foundation/agent-bridle),
in the kernel where the OS allows.

`--full-access` lifts that for one run. The workspace fence, the network
leash, and the exec allowlist are all removed; `write_file` still asks. It is
the `full_access` preset applied to this invocation only.

Native commits still use the attribution and signing broker. For the commit
invocation only, writes are confined to the worktree and its own Git metadata,
keeping the broker helpers immutable. Hooks that write elsewhere need scoped
authority. Governed pushes trust the ambient host installation under full
access, while retaining ownership, file-mode, and destination checks.
Proactive requests for authority already held do not open another prompt.

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
