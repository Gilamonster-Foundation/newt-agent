# Saved workspace access

Use `/settings workspaces` in an interactive Newt terminal to manage saved
workspace profiles. `/workspace` opens the same editor; `/status workspace`
continues to print the current workspace without changing anything. The
Workspaces section of `/settings` also opens the editor.

For a first profile, start with directory-scoped authority:

```sh
newt --workspace-access code /path/to/project
```

The editor requires a trusted operator configuration and signing key, and a
session whose model cannot access private keys or modify the policy, launch
configuration, or Newt executable. If the current session cannot protect those
resources, the editor explains why it is unavailable. It does not reduce or
broaden the running session behind the operator's back.

## Review and save

Choose a workspace, then edit its default working directory, additional read
directories, additional write directories, and command/write preset. Paths are
literal, support spaces, and must name existing directories. Relative paths
resolve from the selected workspace. The editor resolves symlinks to canonical
directories before the final review; the default working directory must remain
inside the workspace.

| Preset | Filesystem and commands |
|---|---|
| Read only | Read the workspace and added read directories; no writes or commands. |
| Workspace edit | Read the workspace and added read/write directories; write the workspace and added write directories; no commands. |
| Workspace development | Workspace edit plus the existing development command allowlist and separately configured extra commands. |
| Workspace full access | All commands under filesystem confinement; read the workspace and added read/write directories; write the workspace and added write directories, including rename and recursive deletion. |

Every profile reads its workspace. Write directories are also readable. A
remaining parent grant still covers its descendants: removing a listed child
does not exclude that child from a broader parent grant. Read-only profiles
cannot contain write directories.

The preview separates current access, the stored profile, and the proposed
next-launch policy. It also shows independent saved approvals and launch
choices. Network permissions and development-command extras remain independent
configuration; the profile does not store them. They are resolved from
configuration on each new launch and included in that session's frozen base
authority. Separate permission approvals can still apply.

**Review and save** presents the resolved candidate. Cancel is the default.
Saving writes exactly that candidate to the existing signed, encrypted approval
store. A concurrent store change causes a refusal and requires a fresh review;
the editor does not merge an unseen change into the confirmation. Before/after
profile states remain in content-addressed, verified history in that encrypted
store. An older wholly valid store can still be restored; the store has no
external rollback-prevention service.

## Applying a saved choice

The saved default working directory sets where `run_command` starts when the
model omits `cwd`. `/cd` changes that command default for the current session.
An explicit `cwd` takes precedence and is workspace-relative or absolute; a
leading relative `cd` in a command starts from that command's effective working
directory. Relative file-tool paths, including `read_file` and `write_file`,
continue to resolve from the workspace root. None of these directory choices
changes the workspace identity or grants additional access.

Saving changes the next launch only. Close Newt and start a **new Newt process**.
`/restart`, a new conversation, and changing the current directory do not reload
the running process's authority. Ending a session is the way to stop its current
access; the editor does not revoke already-issued capabilities or stop jobs.

An optional default workspace is used by `newt` and `newt code` when no directory
is supplied. An explicit `newt code /path/to/other-project` takes precedence.
Each canonical workspace has its own profile. A missing or retargeted selected
directory is refused rather than silently using another policy. Stale entries
can still be removed from the editor without blocking changes to other profiles.

A saved profile selects confined startup behavior and clears inherited
global-access/bypass switches. Explicit `--full-access` or `--yolo` conflicts are
refused. An explicit `--workspace-access` is a one-run override of the saved
command/write preset to workspace full access; it retains the saved directories
and default working directory. Separate `--read`, `--write`, command grants,
prompt approvals, and mode/posture restrictions still apply.

**Remove saved profile** removes that profile and its matching default selection.
Configured permissions then become the fallback. It does not delete independent
saved approvals, erase history, or revoke a running session. Likewise, removing
a directory from a profile does not remove access granted by another approval.
Profiles describe startup policy; they are not a ceiling on later operator
approvals.

## Boundaries that remain

- This is an operator terminal workflow. The model cannot invoke it as a tool,
  and noninteractive use cannot approve or save changes.
- Policy is stored beside the trusted operator configuration, in
  `ocap/session-grants.age`. Ambient repository configuration cannot select this
  authority store. Missing keys for an existing store cause a refusal; Newt
  does not replace them to make the file readable.
- The selected workspace and additional grants must not expose signing or
  encryption keys, or permit policy/configuration/executable modification.
  Parent-directory and alias overlaps are checked. Explicit Build requests are
  also checked against their calibrated toolchain and scratch scope.
- Build/test/format commands can still require Build approval. Networking stays
  separately configured; on macOS the current backend needs explicit network-all
  permission for confined commands. Destination rules and a network monitoring
  console remain [planned work](../design/network-console.md).
- Protected sessions validate a local stdio MCP server's configured executable
  and actual sandbox read roots before startup or reconnect. Advisory-only
  transports are refused. Protected local stdio MCP is currently refused on
  Windows. Parity is planned through an AppContainer stdio launcher with
  explicitly bounded stdin/stdout handle inheritance, which must pass equivalent
  profile and guard tests; that launcher is not implemented here. HTTP MCP is a
  separately authorized remote service, not a local filesystem
  sandbox or an information-flow guarantee against remote deputies.
- Native Git keeps attribution, signing, and default-branch policy. See
  [native commit scope](../design/agent-toolchain.md#native-commit-adapter-scoped-implementation)
  for unsupported families and platform limits. Saving a profile does not add
  platform enforcement capabilities that the selected backend lacks.
- On Windows, a read grant naming only one file cannot provide a verified
  deleted-file snapshot after that file disappears. Progress accounting records
  no deletion progress from that unavailable observation. Workspace and directory
  read grants, including those supplied by profiles, can observe the absence and
  count actual deletion progress. This limitation does not grant access to a
  deleted file's parent or siblings.

For one-run flags and trusted-config examples, see
[full access inside a workspace](full-access.md).
