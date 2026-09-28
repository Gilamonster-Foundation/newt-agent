# agent-bridle-tool-shell

A capability-confined family of shell engines for agent-bridle:

- `BrushShellTool` (`brush`): a carried bash-in-Rust engine running in a fresh
  worker whose static command and file-open filters gate Brush-originated
  external spawns and opens.
- `carried-coreutils`: adds bundled `ls`, `cat`, `echo`, `head`, `sort`, and
  `wc` to Brush. The top-level facade selects it by default on supported
  targets and falls back to `ShellTool` elsewhere.
- `ShellTool` (`shell`): the lean **argv + safe-subset engine** (ADR 0005).
- `HostShellTool` (`host-shell`): full host-shell semantics when the OS sandbox
  can carry the confinement guarantee.

This vendored revision consumes the `brush-ocap-*` prereleases based on the
upstream static-filter API. Its final command policy checks the transformed
program and working directory, requires an explicit environment, and preserves
original spelling separately from executable identity and `argv[0]`. Carried
utilities pass the same final authorization before their private authenticated
spawn. The `exec` builtin remains unavailable inside the private worker because
process replacement would discard its response protocol. External descendants
still require the inherited native sandbox for filesystem confinement.

The safe-subset engine makes agent-bridle the exec funnel: it parses the request
itself, checks the `exec`/`fs` leash, spawns programs directly, and refuses
dynamic constructs by design. It accepts two input shapes:

- argv form (`program` + `args`), and
- free-form `cmd` — a safe subset: pipelines (`|`), redirections
  (`>`/`>>`/`<`/`2>`/`2>&1`), `&&`/`||`/`;` sequencing, filename globbing
  (`*`/`?`/`[…]`), and allowlisted `$VAR` expansion.

Because agent-bridle performs each redirect's open and each glob's directory
listing itself, those filesystem touches are leash-checked (`fs_read`/`fs_write`)
**before any stage spawns**; out-of-scope `exec` is denied at the funnel.

An owning harness may call `ShellTool::with_private_hosts` with exact,
operator-approved hostnames for RFC1918/ULA destinations. The safe-subset
engine's existing managed proxy requires both that approval and the invocation's
ordinary network scope. Approvals are empty by default; the shared core validator
rejects patterns, URLs and endpoint paths. Address screening, pinned dialing,
kernel confinement and proxy lifetime follow the existing core implementation.
This option applies to the safe-subset engine's managed proxy only.

The Brush engine creates its worker through the shared confined-spawn funnel.
Linux and macOS authenticate that private transition over a fresh kernel socket:
the child verifies that the frame sender is its real parent and the exact same
executable image, while core freezes the launch nonce, effective caveats, and
strength floor into a take-once envelope. Knowing the private argv, nonce, or
child-generated challenge is not authority; direct invocation exits 126 before
Brush runs. The worker replaces its control fd 0 with `/dev/null` after the
handshake.

`brush_private_control_supported()` is the construction-time selection probe. It
is true only on Linux and macOS in this release. Other targets deliberately fail
closed rather than treating full access as authentication; a host must select
`ShellTool` there. The top-level facade's default registry performs that
fallback automatically, and its advertised input schema therefore changes from
Brush's full `cmd` grammar to the safe-subset `program`/`args` schema.

When the effective caveats engage an available native L3 backend, the worker,
carried utilities, and all other descendants inherit it; otherwise the result
honestly reports `SandboxKind::None`. It receives only explicit environment
values: the seeded `PATH` and the embedder's `env`-seam variables, which are
exported to the external programs a command runs (nothing ambient is
inherited). Timeout supervision terminates the worker's whole process group.
Restricted filesystem authority fails closed when no kernel backend is
available; `unbridled` is the explicit policy opt-out, not a private-control
bypass.

Each bundled provider is offered as a Brush shim, but Brush's native builtins
win name conflicts, so `echo` normally remains Brush's in-process builtin.
Invoking a non-conflicting utility such as `wc` re-executes the
dispatch-capable embedding binary as
`<current-exe> --invoke-bundled wc ...`; `maybe_dispatch()` recognizes that
private entrypoint before an async runtime is constructed and calls the bundled
uutils implementation. The carried child repeats a PID/image/peer-credential
handshake that binds the exact utility name and raw argv; a direct
`--invoke-bundled` is refused. After acknowledging its argv, the child waits for
an explicit parent confirmation before dispatch. The parent sends it only after
the final live-image check; EOF or an invalid confirmation refuses execution.
This keeps fast utilities alive through authentication without ignoring a failed
identity check. Any L3 policy engaged for the worker therefore
covers the carried child too. A host embedding Agent Bridle must call
`maybe_dispatch()` at the very top of `main`.

When the safe-subset parser refuses dynamic grammar and the `brush` feature is
present, its denial envelope includes a versioned, non-executing
`shell_inspection`. Embedders may also call `inspect_shell` before dispatching
to Brush; the top-level `agent-bridle` facade re-exports that function and its
schema types under the `brush` feature. It preserves the exact source and
provides a flattened inventory of direct command stages, command substitutions,
arithmetic expansions, redirections, quote context, and recognized delegated
descendants. Pipeline and control-flow relationships remain visible in the
exact source but are not encoded as graph edges; warnings call out those
flattened relationships. Arithmetic is projected only when Brush's arithmetic
AST proves it is state-free: integer literals and operators are accepted, while
variables, array references, assignments, and nested shell expansions fail
closed. This matters because bash-like arithmetic recursively interprets shell
state and array indices. The same state-free check covers parameter array
subscripts and substring offsets/lengths. Indirect parameter expansion and the
prompt-expanding `@P` transformation also fail closed because they can
reinterpret a runtime value as expansion syntax. It cannot show a
substitution's value: that value exists only after the inner command runs.
Known dispatch wrappers and interpreters fail closed when their runtime command
cannot be projected. `find -exec` is explicitly marked as delegated because the
external `find` process—not Brush—spawns that child. This projection is approval
metadata, not enforcement or a complete process graph.

The safe-subset path is the **L2 convenience** layer; the subprocess boundary is
L3 (ADR 0005). Coverage is backend- and scope-shaped:

- Landlock confines both filesystem axes, narrows direct exec (reported
  `Interceptor` because the loader trampoline remains), and can deny all TCP on
  ABI-v4 kernels.
- Seatbelt confines both filesystem axes, restricted exec, and empty or
  loopback-only network scopes.
- AppContainer confines filesystem paths, empty or loopback-only network
  scopes, and exec deny-all for supported subprocess engines; the Brush private
  transport is currently unavailable on Windows, so hosts select `ShellTool`.

The result's `sandbox_kind` and per-axis enforcement report are authoritative.
When no backend engages, the kind is `None`; a restricted filesystem scope
fails closed rather than running without its required boundary. The cross-OS L3
strategy is ADR 0009.

- Every engine registers under the same `shell` identity; the embedder selects
  one at registry construction.
- Compiles with the `shell` feature off (exposing only the lightweight observer
  contract), so workspaces build under `--no-default-features`
- Brush remains isolated behind leaf-crate features; `agent-bridle-core` stays
  free of heavy shell dependencies.

## Live output observation

`ShellTool`, `HostShellTool`, and `BrushShellTool` each provide a
`with_output_observer` construction-time builder. The shared
`ShellOutputObserver` receives bounded raw-byte chunks tagged with a process-local
`ShellInvocationId` and `ShellOutputStream::Stdout` or
`ShellOutputStream::Stderr`. Distinct IDs keep concurrent dispatches separate.
The top-level `agent-bridle` facade re-exports all three observer types whenever
any shell-engine feature is enabled.
The ordinary `Registry::dispatch` and `Tool::invoke` paths are unchanged, so
observation adds no authority and cannot bypass the gate.

Chunks may split UTF-8 code points. Callbacks run on a dedicated presentation
thread, so slow observer code cannot stall child pipes, timeout, or cancellation.
Callbacks for one ID are serialized, but different IDs use different presentation
threads and may enter the same observer concurrently. On ordinary completion,
`on_finish(id)` follows every queued `on_output` call for that ID; dispatch does
not wait for it. Cancellation stops accepting new chunks, though a callback
already dequeued may finish afterward. Callback panics are contained.

At most the configured output cap is delivered per stream. For `ShellTool` and
`HostShellTool` this is a live view. The Brush worker's single-response protocol
delivers its bounded captured chunks after the worker completes; it is
observation, not streaming. In every engine the completed envelope is
authoritative. Separate safe-shell pipeline stderr readers follow live reader
scheduling, while that envelope assembles stderr in pipeline-stage order and
then applies its cap, so the two can differ. Excess host/Brush output is drained
and discarded to preserve child behavior without unbounded capture.

Part of [agent-bridle](https://github.com/Gilamonster-Foundation/agent-bridle),
the capability leash for agent tools — a shared, capability-governed tool
registry for the Gilamonster agent line.

## License

Apache-2.0
