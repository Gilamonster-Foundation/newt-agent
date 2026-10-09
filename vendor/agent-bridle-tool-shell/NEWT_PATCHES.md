# Temporary Newt integration patch

Upstream: `agent-bridle-tool-shell` 0.8.0-rc.5 from crates.io. Original
package provenance (`Cargo.toml.orig`), tests, and license notices are retained;
the live manifest and the integration sources carry the changes below.

This copy lets the Lab acceptance fixes build reproducibly without editing a
Cargo registry cache or relying on an unpublished sibling checkout. It is a
temporary dependency override, not a second shell implementation.

Integration changes:

- #2796: Unix Brush and POSIX host entrypoints supply `which` through the
  existing `command -v` builtin, so discovery works without an external which.
  The same wrapper is shared with Newt's ambient Bash/sh route; PATH lookup
  does not confer execution authority. Windows carried discovery is separate.

- #2732: reuse the safe-subset stage supervisor for the sandboxed host shell.
  A configured deadline terminates/reaps the process group and reports a bounded
  timeout envelope. Deterministic tests inject elapsed time into the shared
  supervisor. Descendants that escape their process group remain outside this
  mechanism, as with the existing stage/Brush termination helper. Escaped pipe
  holders can also delay reader joins/return (agent-bridle#420); Windows only
  has direct-child cleanup. SafeSubset now shares one deadline across the
  script and all pipeline stages, cancels and joins the owner on outer expiry,
  and refuses to start later stages after that deadline.

- Project one literal `timeout` wrapper through the existing descendant-command
  inventory, preserving the original shell source and native timeout semantics.
  Recognized flags and option operands, a finite nonnegative duration, and a
  static executable are required. Unknown/dynamic options, nested timeout,
  `find` delegating to timeout, and
  other unprojectable child dispatchers remain unsupported; existing `find`
  child inspection is retained. This inventory does not make native descendants
  Brush-intercepted commands or give them a direct command's broker authority.
- Cancel and reap the actual Brush worker before releasing build scratch.
- Configure bounded output capture and report existing truncation flags so
  Newt cannot claim discarded compiler diagnostics were retained.
- Consume the static command and file-open filter API from the new
  `brush-ocap-*` prereleases. Final execution authorization checks the final
  program and cwd with existing caveats and requires an explicit environment;
  carried utility dispatch preserves the exact authorized builder and private
  authentication protocol. Cancellation uses the generic terminating-error
  marker. The private worker still removes `exec` because process replacement
  would discard its framed response protocol.
- Add an optional host command broker on a separate authenticated socketpair.
  Native commands still run inside the original worker with their original
  stdio; opaque helper requests never execute commands on the host. Exact
  spawned PID, process birth, executable image, and helper parent identity are
  checked before callbacks. A host may declare one exact platform-launcher
  image transition; request bodies cannot nominate images.
- Bound broker frames to 1 MiB and request payloads to 512 KiB, with at most
  64 prepared sessions and 32 concurrent requests per invocation. Every RPC
  uses checked canonical DAG-CBOR and `content-addressable::MerkleNode`: the
  response names its request as a causal parent, and production readers refuse
  invalid content IDs, challenges, or links. These transport records are
  ephemeral; durable domain audit and attribution remain the host policy's
  responsibility.
- Retain broker services and resource leases through worker reaping. Shutdown
  closes pending sockets and joins service threads. Host callbacks must observe
  their supplied cancellation/deadline control. Broker bootstrap receipt and
  worker RPC run synchronously on the single-thread Brush runtime; macOS lacks
  atomic CLOEXEC on SCM_RIGHTS receipt, so that no-concurrent-worker-spawn
  constraint and the parent spawn funnel's ambient-fd scrub are required.
- Read-only mechanism resources are limited to flat directories containing
  aliases of the fixed worker executable. They must be outside all effective
  write authority, including writable relocation ancestors. FS All therefore
  refuses this optional broker; it cannot make helper files immutable. The
  original tool context is unchanged. Broader protected resources and Windows
  authenticated handle delegation remain separate platform work.

The manifest pins parser 0.5.0-rc.1, core 0.6.0-rc.1, builtins 0.3.0-rc.1,
and coreutils-builtins 0.2.0-rc.1 under the `brush-ocap-*` package names. A
prepublication check may use temporary Cargo command-line path overrides;
release acceptance must build from the published packages without those
Brush overrides. This note is not evidence that publication or those checks
have completed.

Upstream these changes into agent-bridle, then remove this override and use
the released dependency. Native Linux and Windows behavior require their own
real-process evidence; macOS results do not establish platform parity.

## Prepared build read handles (#2835)

Brush, Host and SafeSubset accept held read roots from their host caller.
Brush passes them to `SandboxedWorker`, Host to `ConfinedCommand`, and
SafeSubset to `Sandbox::apply_with_held_roots` after checking the admitted
read scope. This retains the existing Landlock descriptor rule through shell
execution without extending tool authority or changing the wire protocol.
Unsupported backends refuse nonempty handle sets. Newt's Seatbelt build route
supplies no handles and explicitly documents its remaining pathname race.
