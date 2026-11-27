# Temporary Newt integration patch

Upstream: `agent-bridle-tool-shell` 0.8.0-rc.7 from crates.io. Original
package provenance (`Cargo.toml.orig`), tests, and license notices are retained;
the live manifest and the integration sources carry the changes below.

This copy lets the Lab acceptance fixes build reproducibly without editing a
Cargo registry cache or relying on an unpublished sibling checkout. It is a
temporary dependency override, not a second shell implementation.

Integration changes:

- #3307: compile the launcher-image helper only for its Linux/macOS production
  callers or Unix tests, and gate its Unix symlink regression to Unix. This
  fixes Windows all-targets compilation without suppressing unused-code
  warnings or changing the Unix assertions. The same fix is owed upstream in
  agent-bridle.

- Backport agent-bridle#527 (`67cf41243dc46ac73ce942a88a070b5907f9dfed`,
  released in `0.8.0-rc.8`): the real Brush test now proves an fs-restricted
  command starts with `sandbox_kind: landlock`, writes inside its grant and
  cannot write outside it. Core removes its obsolete user-namespace refusal.
  This snapshot has no `engine_conformance.rs` or corresponding refusal skip.
  Retain the existing harnessless worker dispatch and all Newt integrations.

- #3220: name the broker RPC socket and operation in I/O failures, including
  macOS `LOCAL_PEERPID` and `LOCAL_PEERTOKEN` lookups, and distinguish the
  delegation channel from the per-request socket. Injected disconnect and
  truncated-frame tests keep these failures fatal; a complete frame followed
  by EOF remains valid. Refuse an exhausted invocation before its server hello
  instead of flooring the remaining budget to 1 ms; retain the 30 s cap and
  recheck the deadline after hello setup. Report server failures on the host's
  captured stderr with the computed timeout in milliseconds, syscall phase,
  I/O error kind and raw errno; malformed-frame diagnostics omit decoder text
  that can echo peer-supplied fields. Pure injected-budget and diagnostic tests
  cover this boundary. Native macOS stress reproduced the descriptor-in-flight
  close race: retain the client's sent socket end without I/O until the server
  hello and both kernel identity lookups succeed. Failures release it by RAII;
  a server that dies before hello now reaches the existing finite read timeout.
  Reply endpoints stay owned until the receiver acknowledges receipt on the RPC
  socket. Worker bootstrap already retains its endpoint through its existing
  authentication ACK (`send_payload_with_control_channel` in core); the borrowed
  `delegated_control::send_control_endpoint` needs no protocol change. The two
  manual sender fixtures also retain their copies until hello receipt. Mocked
  lifetime/error tests and the explicit `broker_transport_real` harness ground
  the ordering, timeout and repeated production handshake (including replies).

- #3093: expose the existing static word parser through a source-bound
  single-argument helper. Broker arguments such as `owner/r` and `done` are
  data, not standalone executable commands; expansion/globs/control syntax
  still refuse. Newt's PR-create and read brokers share this helper.

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

## Locator-stub substitution (#3156)

`BrushShellTool::with_launcher_image_dir` takes the host's own selected
developer-tool directory. When a command names a platform locator stub by
absolute path (Apple's `/usr/bin/git`, `python3`, ...), the worker starts the
same-named tool from that directory instead, provided the exec grant already
admits it. The stub only locates and re-execs the selected toolchain, and inside
the kernel fence it cannot: it needs a per-user cache and a helper the fence
hides, so every absolute-path call exited 71 while a bare name, resolved to the
real tool through the host's PATH, worked.

The engine stays ignorant of the developer directory (no `xcode-select` exec, no
`DEVELOPER_DIR` read, no fixed roots, per `agent-bridle-core`'s trust rules): the
host supplies the directory, the stub is recognised by `is_xcselect_stub` (its
Mach-O imports), the image must be a regular file that is neither the stub nor
another launcher, and the final program is still admitted by
`authorize_external_cmd`. The directory rides the existing authenticated worker
payload as one optional field. Broker preparation and this substitution share
one `rebuild_command` so neither drops argv[0], the environment, the working
directory or a descriptor delegation. Retire this section if the mechanism is
upstreamed.

## Build network diagnostic (#3297)

The supervisor retains the bounded `Build isolation:` pre-exec diagnostic alongside
the authenticated worker transcript. This reports private-loopback versus deny-all
fallback to the operator and model without adding a worker protocol field or
using a diagnostic as authority. The worker's authenticated terminal outcome
remains authoritative.

With agent-bridle#527's obsolete user-namespace refusal removed in core, the
filesystem-restricted Brush worker reaches the same private-loopback pre-exec
hook as argv builds. A successful namespace is supported by the existing
authenticated worker transport; no fallback-only assumption or namespace syscall
backstop is introduced. Newt's real build-pipeline harness can require private
mode and can independently force setup failure to prove the deny-all retry.

## Combined build isolation diagnostic (#3314)

The retained diagnostic now names private loopback plus bounded temporary mounts,
or deny-all fallback without private scratch. It comes from the same pre-fence
child setup as argv builds. Output trimming preserves that effective-mode report;
no worker protocol field or diagnostic-based authority decision is added. Mount
setup failure is exercised separately from namespace failure by the real pipeline
test, through both execution routes.
The caller's directory environment also reaches that same pre-exec initializer:
Brush's full environment payload arrives after startup, too late to recreate a
covered `HOME` or `TMPDIR`. Only the shared fixed directory-variable allow-list
is seeded before exec; no protocol field or loader-control import is added.

## Static shell word classification

Ports the same correction from agent-bridle, whose PR title is
"fix(shell): classify extglob, later brace groups and array words as dynamic".
The brace prefilter spans through the last closing brace, and unquoted
parentheses are treated as dynamic. The existing Brush parser still decides
brace expansion; quoted and escaped words keep their literal values.

The pure program/printf/find regression and the 33-word real-worker
differential test travel with the fix. Newt's independent inspector carries
the same predicates and explicit parity expectations. Downstream Git/gh
policy checks refuse unresolved policy words before command routing.

## Timeout descendant inspection

Ports the same correction from agent-bridle. Both inspection copies refuse
option-like descendant program words, including quoted or escaped forms, and
the unsupported `-f`/`-p` options. `--` before the duration remains supported.
The README documents the bounded GNU coreutils 9.4 grammar and the distinction
between a descendant inventory and execution or broker authority.

Non-foreground wrappers report that GNU timeout's separate process group can
outlive the tool's timeout or cancellation. This change adds no lifetime bound.
Parser regressions, explicit parity expectations and downstream build/broker
tests cover refusals and runtime-dependent words without executing them.

## Covered build inputs (#3337)

Private-build selection now accounts for request read/write roots and directory
environment values before fork. The existing worker bootstrap carries PATH and
tool homes as well as temporary directories; loader and authentication controls
remain excluded. Brush's existing PATH default is made explicit before bootstrap
so inherited search directories participate in the same decision. No authority
record, content identity, or caller grant changes.
