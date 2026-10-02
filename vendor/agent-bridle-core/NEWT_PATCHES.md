# Temporary Newt trusted-control integration patch

Base: the exact crates.io `agent-bridle-core` 0.8.0-rc.5 package. Original
`Cargo.toml.orig`, Cargo package metadata, and license notices are retained.

The privacy-policy citation in `src/net_proxy.rs` belongs to the upstream
workspace, whose documentation is not included in the published crate. Its
[Privacy & the public/private split policy](https://github.com/Gilamonster-Foundation/agent-bridle/blob/23f889b0e0e6648555d043d962a5d0774779d32c/docs/PRIVACY.md)
documents the internal-specifics linter and the distinction between generic
network-range fixtures and private host addresses. The source comment now
points here so that this upstream reference is not mistaken for a Newt document.

This patch adds an optional, explicitly delegated Unix control endpoint to the
existing authenticated worker bootstrap. It does not change the invocation's
filesystem, execution, network, or strength-floor grants. The initial authority
channel is still consumed once and retired; subsequent broker requests use a
separate socketpair. Workers without a broker retain the existing protocol.

The trusted worker can also declare read-only helper resources. This first
contract accepts only flat directories of symlinks to the current worker
executable, outside every admitted write root and relocation path. It rejects
unrestricted writes, unresolved scopes, private stores, and arbitrary resource
trees. Read roots enter the existing RuntimeClosure and the single admitted
mechanism derivation, whose existing content address is verified at apply time.
The original ToolContext and authority payload remain unchanged. This is a
positive read closure for already protected resources, not a deny overlay that
can override an existing filesystem-wide write grant.

`TrustedWorkerRequest::payload` permits read-only inspection of the already
authenticated tool payload before acknowledging its optional endpoint transfer.

Linux and macOS use SCM_RIGHTS with bounded framing, exact descriptor count,
socket validation, and close-on-exec ownership. Windows needs a corresponding
owned-handle transfer using DuplicateHandle and an explicit
PROC_THREAD_ATTRIBUTE_HANDLE_LIST, together with its authenticated worker
transport. That platform path is not implemented by this Unix patch and must
fail closed rather than infer authority from a handle number or pipe name.

Linux receives SCM_RIGHTS with atomic MSG_CMSG_CLOEXEC. macOS has no equivalent;
its receiver marks the owned descriptor immediately, and callers must prevent
concurrent unguarded spawns during that interval. Bridle's parent spawn funnel
scrubs ambient descriptors; the Brush worker receives broker descriptors
synchronously on its single-thread runtime before another command can spawn.

Publication and cross-platform verification are separate required follow-ups;
this provenance note is not evidence that either has completed.

The macOS profile also adds a trusted, default-off `macos_private_ptys` option.
Newt opts in so confined terminal tests can allocate PTYs. It permits the
allocator at `/dev/ptmx` and slave paths only with the kernel's
`com.apple.sandbox.pty` extension. An ambient `/dev` read base no longer exposes
other slave terminals when this option is enabled; explicit read grants remain
effective. Network and executable scopes are unchanged. The Newt real Build
fixture covers private-terminal I/O, unrelated-terminal read/write denial and
the default-off control. This does not provide Linux or Windows device parity.

Seatbelt's existing fence identity records caveats and mechanism, not this
SandboxPolicy option or a complete SBPL ruleset. This patch does not promote
that partial projection to a ruleset-level filesystem proof. Inherited terminal
descriptors and `/dev/tty` remain separate surfaces.

## Backport of agent-bridle#407, pending rc.6

Upstream commit `c6268667f4d1f7bf05253a20686217fabb460dd4` (merged
2026-09-29, "fix(landlock): a git-only exec grant admits git's own exec-path
helpers, execution-free"), applied verbatim to `src/sandbox.rs` plus its
`content-addressable` pin bump (`0.1.0` → `0.1.2`, for `RawContentId`). On
Linux, `landlock_impl::resolve_exec_paths` now also admits `<exec-path>/git`
for a granted `git` whose canonical parent is one of `/usr/bin`, `/bin`,
`/usr/sbin`, `/sbin`, provided both files and their whole ancestry are
root-owned and not group/other-writable and the alias is the same image as
the granted binary. Nothing is executed to decide that, and nothing outside
that one alias is admitted. Closes newt-agent#2630 (`git worktree add` under a
`git`-only exec grant failed with `cannot exec 'branch'`). Remove this section
when the vendored base moves to a release that contains the commit.

## agent-mesh-protocol 0.7

The published 0.8.0-rc.5 manifest requires `agent-mesh-protocol` 0.6.4. This
copy requires 0.7.0, the version the rest of the Newt workspace pins. Without
this change, Cargo resolves the two ranges to two separate crates, and every
`Caveats` crossing between Newt and agent-bridle becomes a type mismatch. Only
the version requirement changed: protocol 0.7 adds `AgentKey::issue_derived`
and removes nothing this crate uses. Upstream agent-bridle needs the same bump
before this vendored copy can be retired.

## Backport of agent-bridle#406 + the #405 promotion/audit, pending both upstream merge and rc.6

Two upstream pieces, backported together because the second depends on the
first:

1. **agent-bridle#406** (`adac41a`, merged 2026-09-29): the zero Mach-lookup
   floor plus named `mach:<service>` grants. Replaces the old ambient
   `MACH_LOOKUP_ALLOWLIST` ("ADR 0015 E4") with a default-deny floor: nothing
   is ambient, and a service is reachable only via an explicit `mach:` grant
   in the `net` scope, projected as the named class
   `seatbelt-mach-service:<name>` (never `∅` while a grant exists). Ships
   `MACH_DEPUTY_AUDIT = Incomplete`, so every restricted Seatbelt `net` shape
   still resolves `Unknown` and is refused — this alone changes nothing about
   what admits.
2. **The #405 promotion** (agent-bridle branch `fix/405-macos-net-none-audit`,
   head `97f031f` at backport time — **NOT YET MERGED to agent-bridle main,
   no PR opened yet**): the completed native channel audit (ADR 0015
   amendment E6 — unix-domain sockets, `open(1)`/LaunchServices, Darwin
   notifications, pasteboard, `iokit-open`, `sysctl-write`, a write-class
   `file-ioctl`, `process-info`/`signal`, XPC beyond `mach-lookup`, and
   AppleEvents, each shown closed or correctly placed out of the net-egress
   threat model) and the resulting flip: `MACH_DEPUTY_AUDIT = Complete`. This
   is what actually changes behavior: a `net: none` (or `unix:`-only,
   `mach:`-only, or a mixture of only those) Seatbelt scope now resolves
   `Bounded(∅)` at the L3 scope bound (`resolved_authority`) AND reports
   `Kernel` at the L4 strength floor (`report.rs`'s `enforcement_report`,
   gated on the SAME audit-complete predicate) — together, not one without
   the other. A named `mach:` grant resolves to its own class (never `∅`,
   never `Unknown`); a loopback scope or a general remote-host allowlist are
   unaffected and stay `Unknown`/`Advisory`.

**Why newt needs this**: newt-agent#2673. On macOS, `ConstrainedExecutor`
narrows every `run_command`/MCP-spawn's net scope to `net: none` before
spawning (`NetGrant::DenyAll` → `deny_all_net`, `confined_exec.rs`) —
independent of whatever the operator's config actually named. Before this
backport, the vendored `SeatbeltSandbox::resolved_authority` mapped EVERY
restricted `net` shape (`net: none` included) to `Unknown`, so
`AdmittedFence::admit`'s L3 scope-bound check refused every confined macOS
spawn with "not decidable against the delegated grant ∪ declared runtime
closure (L3 BOUND)" — `run_command`, MCP servers, and git all failed. Linux
was unaffected (a separate, independent `newt-net-guard` seccomp floor
supplies its own Kernel witness there, outside this mechanism entirely).

**What was backported, mechanically**: `src/sandbox.rs` and `src/report.rs`
replaced with the upstream branch's content for these two files (verified:
newt's one local addition in `sandbox.rs`, `net_unix_only` — a `unix:`-only
predicate without `mach:` support — is strictly superseded by upstream's more
general `net_direct_denied`, used at the exact same two call sites in the
new code; nothing is lost). `src/spawn.rs` is a 3-way merge
(`git merge-file`) against upstream's pre-#406 base
(`9df604c08102175b3a2e03524b1b46b9f186ea4a`): clean, no conflicts, newt's
trusted-worker-broker patch is untouched. `tests/unix_socket_grants.rs` is
the straight upstream diff applied (bases were byte-identical). `src/lib.rs`
gains the matching public re-exports (`mach_service_grants`,
`seatbelt_mach_service_class`, `MachServiceDisclosure`,
`mach_service_disclosure`, `MACH_GRANT_PREFIX`, `MACH_SERVICE_CANDIDATES`) —
without them the standalone crate check flags all six as dead code, since
the Seatbelt-only call sites that would otherwise use them are platform-gated
out on Linux. No version bump: `content-addressable` and
`agent-mesh-protocol` pins are unchanged.

**Status**: both the agent-bridle PR for `fix/405-macos-net-none-audit` and
this newt backport are unopened, per operator instruction, pending review.
Remove this section (and fold the Mach-floor logic back into a plain `rc.6`
bump) once agent-bridle's own release contains both #406 and the #405
promotion.

## Descriptor-bound read roots

`HeldReadRoot`, `Sandbox::apply_with_held_roots` and
`ConfinedCommand::held_read_roots`: a caller that already holds a read root as
a directory descriptor (and has verified its identity) hands a duplicate to the
spawn. The Landlock ruleset adds `PathBeneath` on that descriptor and drops the
path-opened rule for the same root, so a swap at the pathname after the
caller's check cannot re-point the fence. Admission is unchanged — the root is
still spelled in `fs_read` — only how the rule is built changes. A wrapper
backend (Seatbelt) refuses a non-empty set rather than fall back to the path.
Newt's governed push uses this for the confined object copy. Filed upstream as
a patch; retire this section when it lands.

Hardened (#2674 P1): the two claims above — "admission unchanged" and "a
caller that has verified its identity" — are now enforced, not merely
documented preconditions. `HeldReadRoot::bind` (the only constructor; fields
are private, no public struct literal) refuses unless `fd`'s `(dev, ino)`
equals what `provenance` names right now, so a falsely labelled
`HeldReadRoot(A, fd(B))` cannot be constructed. `ConfinedCommand::spawn`
separately refuses any held root whose `provenance` is not a member of the
admitted `fs_read` scope, before the spawn thread starts, so a genuinely
bound but un-admitted `HeldReadRoot(B, fd(B))` cannot reach the sandbox
either.
