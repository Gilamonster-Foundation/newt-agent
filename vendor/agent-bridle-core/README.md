# agent-bridle-core

The capability-enforcement core of agent-bridle: the `Tool` trait, the `Gate`
(the single mint site for a `ToolContext`), the `Registry`, the `Sandbox`
plumbing, and the `ToolEnvelope` result type. It re-exports the canonical
authority types (`Caveats`, `Scope`, `CountBound`) from `agent-mesh-protocol`
so every host and tool speaks one lattice.

The non-bypassable invariant: a `Tool` can only act through a `ToolContext`,
and a `ToolContext` can only be minted inside `Gate::authorize`. The tool
receives the *meet* of granted-and-required authority — least authority by
construction.

- `Tool` / `Registry` — declare required `Caveats`, dispatch through the gate;
  `Registry::dispatch_with_strength_floor` binds a host-approved confinement
  minimum to the actual invocation
- `Gate` + `ToolContext` — mint-token enforcement; no public constructor
- `Sandbox` — honest `NoopSandbox` fallback plus opt-in native Landlock,
  Seatbelt, and AppContainer process boundaries
- `LocalExecutionBackend` + `ExecutionHandle` — the backend-neutral managed
  execution lifecycle (#370). `ExecutionRequest` carries mechanism inputs only
  (executable, argv, cwd, explicit env, stdin, limits) and **no authority**:
  starting an execution requires a `ToolContext` and goes through the same
  `ConfinedCommand` admission → sandbox → `verify_applied` funnel as every
  other confined spawn, with the applied `AdmittedFenceId` carried into
  `Started` and the final evidence. The stream is `Accepted`, `Started`,
  ordered stdout/stderr, `OutputTruncated`, denials, and exactly one terminal
  (`Exited`/`Denied`/`Failed`) under one strictly increasing sequence, with
  idempotent `wait`/`cancel`/`kill` and a drop that terminates and *joins* the
  process tree rather than detaching it. Buffering is physically bounded by
  queued event count *and* queued bytes, with exact dropped-byte accounting;
  the terminal is held out of band so a full queue cannot lose it. `Exited` is
  published only after the tree is reaped, both pipes are at EOF, and any
  egress proxy has been joined to quiescence via `ProxyHandle::shutdown_and_join`
  — a proxy that cannot be finalized becomes `Failed`, never a successful exit.
  Only Local is implemented: a remote fence needs the sandbox-grain
  identity/provenance binding of RFC 5b first, so there is no
  execution-location axis on `ConfinedCommand` to route on.
- `step_up` — human-presence step-up (the `attest` outcome): `Gate::evaluate` / `authorize_with_discharge` / `authorize_step_up`, the `DischargeProvider` ceremony seam and `DischargeVerifier` proof check. The production `Ed25519Verifier` is behind the off-by-default `verifier-ed25519` feature; WebAuthn EdDSA and ES256 assertion verifiers are behind `verifier-webauthn` and `verifier-webauthn-es256` (ADR 0007)
- Deliberately tiny dependency budget (`anyhow`, `serde`, `serde_json`, `async-trait`, `agent-mesh-protocol`); no tokio by default — heavy runtimes live in leaf tool crates. Optional, off-by-default deps include `landlock` (`linux-landlock`) and `ed25519-dalek` (`verifier-ed25519`)

## Features

| Feature | Default | Pulls | Enables |
|---|---|---|---|
| `linux-landlock` | off | `landlock` (Linux only) | filesystem confinement, direct-exec narrowing, and deny-all TCP on ABI-v4 kernels |
| `macos-seatbelt` | off | no Rust dependency | `sandbox-exec` filesystem/exec confinement; expressible restricted-net profiles (deny-all, loopback, and exact `unix:` socket endpoints) add direct-network rules and `net:none`/`unix:`-only also add a Mach floor, but all remain `Unknown` and are refused |
| `windows-appcontainer` | off | companion `agent-bridle-aclaunch.exe` | AppContainer filesystem DACLs, deny-all or loopback-only network policy, and exec deny-all |
| `os-sandbox` | off | target-specific backend deps | convenience feature for every native OS sandbox backend |
| `verifier-ed25519` | off | `ed25519-dalek` | production `Ed25519Verifier` for step-up discharges |
| `verifier-webauthn` | off | `ed25519-dalek`, `sha2` | production WebAuthn EdDSA/-8 assertion verifier |
| `verifier-webauthn-es256` | off | `p256`, `sha2` | production WebAuthn ES256/-7 assertion verifier |

Coverage is scope-shaped and is surfaced per axis. Landlock's loader-trampoline
residual keeps `exec` at interceptor strength; Seatbelt confines restricted
exec; AppContainer reports exec as kernel only for deny-all. General hostname
network allowlists are not directly expressible by the native kernels. Their
support is backend-specific, and the macOS proxy path is currently held. A
restricted filesystem scope fails closed when no native backend can enforce it.

On macOS, the E4 `net:none` Mach floor closes the demonstrated NSURLSession /
`nsurlsessiond` deputy as defense in depth. It does not establish deputy-complete
no-egress: allow-listed and other ambient IPC services have not been
comprehensively certified. Consequently every restricted Seatbelt `net` scope,
including `net:none`, loopback, and the former loopback-proxy shape, resolves
`Unknown` and is refused by admission.

An explicit `net` scope entry `unix:/absolute/canonical/service.sock` names a
single existing Unix-domain socket; the path must already be canonical and
name a socket — relative paths, symlinks, missing paths, and patterns fail
closed. The profile emits an exact outbound exception for that path and
nothing wider; filesystem authority stays independent, and a mixed
remote-host/socket scope requires the managed async egress proxy (Unix
entries never enter its DNS host list). Every other backend refuses a `unix:`
grant outright, including at the advisory strength floor. Like every other
restricted Seatbelt `net` shape above, admission currently resolves this one
`Unknown` too and refuses it — the profile machinery is real, but the same
hold applies until the outstanding confinement proof is accepted.

On Windows, AppContainer is attached at process creation by the wired
`agent-bridle-aclaunch.exe` wrapper rather than by `Sandbox::apply` on the
current thread. The wrapper is trusted only when it is shipped next to the
current executable, or when `SandboxPolicy::appcontainer_launcher_path` names an
explicit absolute helper path; the AppContainer backend never searches ambient
`PATH` for its sandbox constructor.

## Exact private-host approvals for async subprocesses

With `spawn-tokio`, an owning harness may call
`ConfinedCommand::with_private_hosts(["service.example".to_string()])?`
before `spawn_tokio`. This separate, transient approval lets that exact name
resolve to RFC1918 or IPv6 unique-local space only when the active context's
ordinary network allowlist also permits it. Supply names approved by the
operator, never names taken from tool output or server metadata.

The default is empty. Wildcards, URLs, ports and malformed names are rejected;
DNS names use ASCII/ACE labels with case and final-dot normalization. Metadata,
link-local, unspecified, multicast and CGNAT destinations remain blocked.
The existing proxy resolves each connection once and dials that screened
address. Its kernel loopback fence and the command's filesystem/exec caveats
are unchanged. Platforms without that fence retain their documented advisory
posture; this option does not enable a proxy or alter synchronous `spawn`.

`net_proxy::start_with_private_hosts` exposes the same policy for embedders
already using the proxy's resolver/audit seams. Existing constructors retain
the default private-address denial.

Private-host approvals are honoured by `spawn_tokio` and `ShellTool`, not yet
by `spawn_managed_authorized`.

## System timezone data

The macOS read baseline includes the OS-owned timezone database, so confined
programs can follow `/etc/localtime` and `/usr/share/zoneinfo` into the standard
data files. Linux already includes its localtime file and shared runtime data.
These are read defaults in `SandboxPolicy::base_read_paths`, not write grants.
Setting `TZ` to another file never grants access to that file.

`ConfinedCommand` still starts with an empty environment. A host that inherits
the parent's explicit timezone must pass `TZ` through its existing `env` seam,
preserving absent versus empty values. The runtime interprets the value.
Windows AppContainer timezone parity is tracked in [#387](https://github.com/Gilamonster-Foundation/agent-bridle/issues/387);
micro-VM named-zone materialization is tracked in [#388](https://github.com/Gilamonster-Foundation/agent-bridle/issues/388).

Part of [agent-bridle](https://github.com/Gilamonster-Foundation/agent-bridle),
the capability leash for agent tools — a shared, capability-governed tool
registry for the Gilamonster agent line.

## License

Apache-2.0
