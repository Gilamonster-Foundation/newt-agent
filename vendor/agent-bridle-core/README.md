# agent-bridle-core

The capability-enforcement core of agent-bridle: the `Tool` trait, the `Gate`
(the single mint site for a `ToolContext`), the `Registry`, the `Sandbox`
plumbing, and the `ToolEnvelope` result type. It re-exports the canonical
authority types (`Caveats`, `Scope`, `CountBound`) from `agent-mesh-protocol`
so every host and tool speaks one lattice.

Trusted-worker image binding (PR #419) is specific to the Linux Landlock
backend. The worker is opened once through the held-root resolver and must be
a regular file. Landlock read/exec exceptions use that descriptor, and execution
uses its `/proc/self/fd` operand with close-on-exec retained. Path replacement
cannot change the launched object or widen the exception to a directory; this
pins an inode, not immutable contents. Missing procfs or unsupported descriptor
execution refuses the Landlock launch, without falling back to the pathname.

Seatbelt, MinimalRootfs, and MicroVm retain their pre-PR exec-only worker
closure: no worker image read grant is added. AppContainer and Noop declare
no worker closure. These backends retain their platform launch routes without
Linux's descriptor-execution path or its image-binding guarantee.

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
| `macos-seatbelt` | off | no Rust dependency | `sandbox-exec` filesystem/exec confinement; expressible restricted-net profiles (deny-all, loopback, exact `unix:` socket endpoints, and named `mach:` service grants) add direct-network rules and the network-denied shapes also install a zero Mach-lookup floor. Only the zero-grant `net:none` shape (unprivileged caller, audited stdio) reaches kernel strength; `unix:`/`mach:` grants project exactly but report `Advisory` (they fail only a `Kernel` net floor); loopback and remote-host scopes, and any root or unaudited-stdio launch, resolve `Unknown` and are refused at admission (see below) |
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

On macOS, a network-denied profile (`net:none`, or `unix:`/`mach:` entries
only) installs a **zero** Mach-lookup floor (agent-bridle#405): every named Mach
service is kernel-denied unless the operator grants it by name with a
`mach:<global-name>` scope entry, for example
`mach:com.apple.system.opendirectoryd.libinfo`. Nothing is ambient; the former
compatibility allow-list is now the documented candidate set
(`MACH_SERVICE_CANDIDATES`) a host may offer for a grant. The result envelope's
`disclosure.mach_services` names what was granted and which candidates were
withheld — the structured "service X denied" signal, derived from the installed
policy because the kernel denies a Mach lookup silently. With no grant the
floor closes every named Mach lookup (the demonstrated NSURLSession /
`nsurlsessiond` path included); a grant re-opens the named service, deputy or
not. The deputy audit for the deny-all shape is **complete** (`MACH_DEPUTY_AUDIT`,
`sandbox.rs:2862`; evidence in ADR 0015 amendment E6), narrowed to what was
measured, so it is a bounded claim and not a proof that every ambient IPC route
is closed. Consequently, on macOS:

- **Prerequisites for every bounded projection.** The caller must not be root
  and the spawn's stdio must be audited: a pipe or `/dev/null` for each of
  stdin/stdout/stderr, plus the trusted-worker's one-shot authenticated control
  channel (#417), which the worker retires before any governed action. A
  root-owned caller or unaudited stdio (an inherited or redirected descriptor)
  makes the net scope `Unknown` (`resolved_authority`, `sandbox.rs:3091-3096`),
  so it is refused at scope admission **regardless of the requested strength
  floor**, for `net:none`, `unix:` and `mach:` shapes alike.
- **`net:none` (zero grants) is promoted.** With the prerequisites met, the
  projection resolves it to the empty scope (`sandbox.rs:2873`) and the report
  grants the net axis `Kernel` strength (`report.rs:435`), so it also satisfies
  a `Kernel` net floor.
- **`unix:`-only, `mach:`-only and mixed shapes**, with the prerequisites met,
  resolve to exactly what the profile permits (the `unix:` endpoints as concrete
  entries; each `mach:` grant as the named class `seatbelt-mach-service:<name>`,
  never `∅`), so the scope bound compares them honestly and they can be
  admitted at an `Advisory`-compatible floor such as the default. Their
  enforcement report stays `Advisory` (the audit proved only the zero-grant
  shape), so only a `Kernel` net floor rejects them.
- **Loopback and remote-host allow-lists** (and the former loopback-proxy
  shape) resolve `Unknown` under either state and are refused at scope
  admission regardless of the floor: their Mach lookup is ambient (no floor),
  so nothing bounds a deputy. Direct loopback rules are real, but they are not
  an audited bound.

A `mach:` grant mixed with a remote-host allow-list has no
egress-proxy semantics and the proxy planner refuses it.

An explicit `net` scope entry `unix:/absolute/canonical/service.sock` names a
single existing Unix-domain socket; the path must already be canonical and
name a socket — relative paths, symlinks, missing paths, and patterns fail
closed. The profile emits an exact outbound exception for that path and
nothing wider; filesystem authority stays independent, and a mixed
remote-host/socket scope requires the managed async egress proxy (Unix
entries never enter its DNS host list). Every other backend refuses a `unix:`
grant outright, including at the advisory strength floor. With the prerequisites above met, its scope resolves to the concrete socket
entries and it can be admitted at an `Advisory`-compatible floor; its
enforcement report stays `Advisory`, so a `Kernel` net floor refuses it.

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
