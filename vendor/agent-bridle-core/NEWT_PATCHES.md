# Temporary Newt trusted-control integration patch

Base: the exact crates.io `agent-bridle-core` 0.8.0-rc.7 package. Original
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

## Worker-control stdio audit backport (agent-bridle#417, 2026-10-02)

Cherry-picks agent-bridle PR #417 (ADR 0015 amendment E7, open upstream at the
time of this backport — not yet merged): adds `ConfinedStdio::WorkerControl`,
a fourth named/audited `ConfinedStdio` variant for the trusted-worker's
one-shot authenticated control-socket stdin (`WorkerControlHandle`,
module-private constructor — narrowed from crate-private in round 2, since
its only call site is `SandboxedWorker::spawn_supported` in the same
module), and wires `SandboxedWorker::spawn_supported`'s stdin/stdout/stderr
through the named variants instead of raw `Stdio`. Fixes `run_command`
(Brush) refusing on macOS under `net: none` even for commands that touch no
network — "backend authority on the Net axis is not decidable ...(L3
BOUND)" — since the worker's stdin never claimed the audited stdio shape
this crate's own `net:none` Seatbelt promotion (the `#406`/`#416` backport
above) requires. See the upstream PR for the full argument and agent-bridle's
ADR 0015 (ignore this file's own copy drift; the PR is authoritative).
Does NOT resolve a RAW host net allow-list handed directly to this crate —
newt's own `shell.rs` (`dispatch_caveats_for_command`/`spawn_net_scope`)
narrows any non-empty host-list `net` grant to `net: none` before calling in,
so `run_command` here only ever sees the `net: none` shape this fix
resolves; see the PR body's "Scope note" / "Out of scope" sections.

**Round 2 (2026-10-02):** `agent-bridle-tool-shell/src/host_shell.rs`
(ADR 0019's separate "sandboxed-host" engine) still built its
`ConfinedCommand` with raw `Stdio::null()`/`Stdio::piped()` — upstream's own
copy was already migrated by `#416`, but that migration was never part of
`#417`'s cherry-pick above, so it was missed when this vendor was first
synced. Backported now: named `ConfinedStdio::Null`/`Piped`. A new pair of
tests in `tests/host_shell_real.rs` exercises the `ConfinedCommand` shape
`host_shell.rs` builds (`/bin/sh -c <cmd>`, its default `SandboxPolicy`)
directly against a `net: none` context — `HostShellTool::invoke` itself can
never reach a restricted-net admission check this way; ADR 0019 D2 refuses
any restricted `exec`/`net` grant before a spawn is built at all — so this
is a shape/hygiene fix (matching upstream, claiming the audit credit the
mechanism already allows), not a user-visible behavior change through
`HostShellTool`'s own API. Measured on the Mac test runner (macOS 15.7.3,
Apple Silicon): both the new named-stdio admit test and the raw-`Other`
refuse test pass identically whether `host_shell.rs` carries this fix or
not, confirming they characterize the `ConfinedCommand` mechanism for this
invocation shape rather than regression-testing `host_shell.rs` itself.

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

## Final agent-bridle#419 backport (2026-10-04)

Backports merged upstream commit
`48ecd0e8d827a9d2c6057f381f531befa2b1497c`:
<https://github.com/Gilamonster-Foundation/agent-bridle/pull/419>.
This replaces the original #418/#419 pathname-read backport. Landlock binds a
regular worker image once through the existing held-root resolver, installs
read/exec rules on that descriptor, and executes the same inode. Binding or
descriptor execution failures refuse; there is no pathname fallback. Seatbelt,
MinimalRootfs and MicroVm retain exec-only worker closures; AppContainer and
Noop retain empty worker closures. The image's contents are not immutable.

File-by-file adaptations against that upstream commit:

- `src/spawn/worker_image.rs`: identical upstream content.
- `README.md`: identical #419 worker-binding paragraphs; retain the existing
  vendor README elsewhere (its older Mach-network discussion is pre-existing
  documentation drift outside this backport).
- `src/admitted.rs`: no new changes needed. The existing vendor `fs_read`
  set, builder, emptiness check and admission merge already implement the
  upstream additions for helper resources. Preserve the vendor's narrower
  builder visibility, Windows path-separator normalization, and existing
  admission/projection integration.
- `src/spawn.rs`: retain trusted worker-control/broker and validated helper
  read resources, adding their roots after the bound-image closure. Preserve
  held-root scope validation and wrapper refusal. Pass the held roots alongside
  the worker descriptor at apply; ordinary spawns retain `apply_with_held_roots`.
  All upstream startup, replacement, sibling/directory/symlink and non-Landlock
  closure regressions are retained. One vendor-only regression replaces both
  worker and held-root paths after admission and requires the original image
  to read the original held directory.
- `src/sandbox.rs`: the vendor's `HeldReadRoot` API remains. Extend upstream
  `apply_with_worker`/`apply_rules` with a held-root slice so one Landlock
  ruleset installs both bindings. Remove the worker label before pathname
  derivation, and retain held-root pathname removal before read rules. Keep
  the vendor's other existing policy adaptations, including private PTYs.
- `Cargo.toml`: retain the crates.io-normalized manifest, protocol 0.7 pin and
  existing dependencies/features; add only upstream's rustix `fs` feature.
- `src/lib.rs`: rustfmt-only wrapping of existing exports; no API change.
- `Cargo.lock`: align standalone `async-trait` to the workspace
  `0.1.92` pin (and its `syn` dependency). The old standalone `0.1.89`
  macro triggers Rust 1.99 `double_must_use` errors under clippy -D warnings.
  The workspace lockfile is unchanged.

The vendored crate is excluded from the workspace, and workspace CI does not
run its unit/integration tests. Validate directly with `cargo test
--manifest-path vendor/agent-bridle-core/Cargo.toml` in both feature modes;
workspace dependent tests are additional evidence, not a substitute. Remove
this section when the vendor is replaced by a release containing #419 and
its remaining Newt-specific integrations.

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

## Descriptor-bound read roots (vendor-only; rc.6 has no `HeldReadRoot`)

`HeldReadRoot`, `Sandbox::apply_with_held_roots` and
`ConfinedCommand::held_read_roots`: a caller that already holds a read root as
a descriptor (and has verified its identity) hands a duplicate to the spawn.
The Landlock ruleset adds `PathBeneath` on that descriptor and drops the
path-opened rule for the same root, so a swap at the pathname after the
caller's check cannot re-point the fence. Admission is unchanged: the root is
still spelled in `fs_read`; only how the rule is built changes. A wrapper
backend (Seatbelt) refuses a non-empty set rather than fall back to the path.
Newt's governed push uses this for the confined object copy.

Invariants (#2674 P1), enforced, not documented preconditions:
- `HeldReadRoot::bind` is the only constructor (private fields, no public
  literal). It refuses unless `fd`'s `(dev, ino)` equals what `provenance`
  names right now (`fstat(fd)` vs `stat(path)`; the path is never reopened), so
  a falsely labelled `HeldReadRoot(A, fd(B))` cannot be built.
- `ConfinedCommand::spawn` refuses any held root whose `provenance` is not a
  member of the admitted `fs_read` scope, before the spawn thread starts, so a
  genuinely bound but un-admitted `HeldReadRoot(B, fd(B))` cannot reach the
  sandbox.
- Landlock chooses ReadFile for non-directory objects and ReadFile|ReadDir for
  directories from `fstat` of the held fd. A regular-file root never grants
  its parent directory. A real Landlock regression holds a regular file across
  replacement and checks that the original inode stays readable while the
  replacement and a sibling remain denied.

Newt #2835 extends the mechanism through `SandboxedWorker::held_read_roots`.
The worker delegates the handles to its existing `ConfinedCommand`; admission
and wrapper-backend refusal stay there. No handles or new fields are
serialized into the worker protocol. Composing held roots with #419's worker
binding keeps both original objects after their pathnames are replaced
(#2692). Retire this section if the mechanism is upstreamed.

Newt #3156 makes `xcselect::is_xcselect_stub` public (`pub use` in `lib.rs`). The
shell tool's launcher substitution recognises Apple's locator stub by identity
rather than by a fixed path. The function and its checks are unchanged.

## Brush user-namespace refusal backport (agent-bridle#527, 2026-11-26)

Backports upstream commit `67cf41243dc46ac73ce942a88a070b5907f9dfed`, released
in `0.8.0-rc.8`. Remove the filesystem-restricted Brush worker refusal and
`linux_user_namespaces_hardened` sysctl probe. Upstream corrected its claimed
bind-mount escape: a thread under a Landlock filesystem ruleset cannot mount,
even with capabilities in a new user namespace. No namespace syscall backstop
is needed for that filesystem boundary.

Port the upstream bind-mount proof and its required-prerequisite test into the
existing Landlock child tests; the shell vendor carries the accompanying
filesystem-restricted Brush proof. The mount proof first checks direct read
denial, then requires an unconfined positive control before testing the confined
mount. `BRIDLE_REQUIRE_USERNS=1` and `BRIDLE_REQUIRE_LANDLOCK=1` make unavailable
prerequisites fail instead of silently leaving the namespace path unproven.
The upstream conformance skip is absent from this vendor snapshot, so there is
no skip to remove. Preserve Newt's held roots, bound worker image, helper read
resources and authenticated worker-control integrations. No grant, protocol,
record, dependency version or network policy changes.

Run the excluded vendor crates through their own manifests, with Cargo path
patches for this core and `agent-bridle-fdguard`; the workspace patch table does
not apply to standalone vendor tests. Retire this section when the vendor base
includes #527.

## Confined build private loopback (#3297)

`ChildNetworkPolicy::PrivateBuild` (formerly `PrivateLoopback`) is selected only
by Newt's deny-all build requests. The Landlock rule builder prepares both base filesystem
rulesets before fork: one with the original TCP deny and one without it. The
existing fdguard seam creates a user/network namespace and brings up loopback
before installing the chosen rules and seccomp program. The namespace has no
host interface or external route. Namespace setup errors occur before exec;
the spawn funnel retries once in a fresh child with the original deny-all floor.
This avoids carrying partially mapped user namespaces into fallback builds.

Both choices retain the filesystem rules, descriptor hygiene and io_uring deny.
A separate network-only Landlock domain is deliberately avoided: its implicit
REFER denial would break cross-directory build renames. Ordinary shell/git
policy and real network grants are unchanged. The existing admitted-fence
content identity and apply verifier remain the authority handoff.

This composes with the #527 refusal removal above: the Brush worker and argv
child use the same `ConfinedCommand` pre-exec setup. Setup completes before
either seccomp or Landlock and before any worker/build code executes; seccomp
sets no-new-privileges before Landlock is installed. No production namespace
syscall backstop is added. Child capabilities belong to the new user namespace,
not the host; subsequent namespace creation cannot remove either inherited fence
or import host network devices. Landlock still refuses mount-topology changes.

The network namespace isolates host internet and abstract Unix sockets, and
the descriptor guard closes inherited host descriptors. Pathname Unix socket
deputies remain the existing `local-deputy-egress` limitation; this patch does
not claim to close that separate IPC surface.

## Delegated control descriptor cleanup (newt #3311, 2026-11-26)

Carries the receiver fix from agent-bridle commit
`7a8bbf03c62ff2d700079e66eb87ac787c30d173`, prepared for upstream review.
`src/delegated_control.rs` matches that source, including its named tests.
Darwin installs an entire SCM_RIGHTS batch before truncating ancillary copyout;
receive enough space for the kernel-supported batch so every installed
file descriptor is owned and closed when an excess count is refused. Receive
all marker fragments with recvmsg, rejecting extra rights on the marker tail.

The added tests belong to the standalone vendor's real-kernel suite and run
serially. They ground the receiver's exact-count and owned-cleanup contract with
isolated descriptor-table snapshots and peer EOF before and after socket drop;
they do not substitute mocked evidence for kernel behavior. The maximum-batch
and marker-tail cases also reproduce the defects on Linux. Native macOS
validation of this vendor port remains a separate required check.

No authority, framing discriminator, dependency, or public API changes. Retire
this section when the vendored base contains this fix. Standalone vendor tests
need the Cargo path override for `agent-bridle-fdguard`; the workspace patch
table does not apply to the excluded manifest.

## Private build scratch (#3314)

The same child setup now creates a mount namespace, makes propagation recursively
private, and mounts bounded tmpfs instances at `/tmp` and `/dev/shm`. The private
ruleset receives read/write rules from directory handles opened after mounting;
the fallback ruleset never receives them. The access mask comes from the existing
filesystem rule builder. No host temporary directory is added to a path grant.
Setup failures discard the child before exec; fence installation failures refuse
execution. Workspaces beneath the covered roots retain the original fallback.
The existing trusted-worker builder carries only directory environment values
into private-build setup, before the later authenticated payload is read. The
shared fdguard allow-list excludes loader controls and the worker nonce; normal
workers and real network grants retain their empty bootstrap environment.

`PrivateBuild` names both network and scratch isolation. The serialized field
shape is unchanged and the old `private-loopback` tag is accepted as an alias;
new output uses `private-build`. Older readers cannot decode that new tag. The
existing canonical fence identity therefore distinguishes the broader mechanism
from old network-only evidence, and its production verifier remains in place.

Namespace setup changes no cgroup membership and grants no host-namespace capability.
Landlock prevents mounting after the fence, including in later namespaces.
Temporary filesystems disappear when their final namespace/mount references are
released. See the fdguard patch note for resource bounds and environment handling.

## Private build PTYs (#3320)

`PrivateBuild` also supplies bounded private devpts through the same fdguard
setup. No new configuration field or host terminal grant is introduced. The
prepared access mask carries IOCTL_DEV only when the configured ABI handles it;
fdguard restricts the new terminal rules to read/write and applicable ioctl rights.
The private-build seccomp filter denies TIOCSTI and TIOCLINUX, while preserving
PTY allocation, terminal settings and window sizing. Ordinary execution retains
its existing policy. See the fdguard patch note for setup, bounds and fallback.

## Same-object Git alias in private builds (#3318)

Inside a private build's user namespace, files owned by host root show as the
overflow uid, so `git_exec_path_binaries` cannot establish that Git's exec-path
alias is root-owned and a nested `git`-only fence refuses Git's own re-exec. The
root-ownership rule is unchanged, and no uid is treated as root.

`held_git_alias_binds` runs in the host namespace, in `apply_rules`, before the
build child exists. For each `git` in `GIT_TRUSTED_DIRS` it takes the alias the
existing rules admit and verifies it again on held descriptors: each path is
walked from `/` through directories that stay open while the next component is
opened beneath them with symlinks refused, every component is `fstat`ed for
uid 0 and no group/other write, both ends must be regular files and different
objects, and the bytes read from the two held descriptors must have one
`RawContentId`. A verified pair goes to the fdguard hook, which attaches the
primary read-only over the alias in the build's mount namespace (see the fdguard
patch note). An alias claimed by two different Gits is bound to neither.
Anything unverified stays unbound, which is the previous behaviour.

Inside the build the alias and the primary are then one kernel object, so the
nested fence's ordinary rule for the granted primary covers both names. The
resolver gains one arm, `is_spelling_of_granted_object`: a candidate that is the
same device and inode as the granted Git, with no group- or other-writable
component on either path, is listed, so the projection names the spelling the
kernel rule already admits. That arm adds no authority and judges no other
file; a different object at the alias is still admitted only by the root-owned
same-image rule. Visible effect: within a private build the alias path has the
primary's inode. The host, the deny-all fallback and non-build spawns are
unchanged. Retire with the upstream port of this mechanism.

## Covered build inputs (#3337)

Private-build selection now accounts for request read/write roots and directory
environment values before fork. The existing worker bootstrap carries PATH and
tool homes as well as temporary directories; loader and authentication controls
remain excluded. Brush's existing PATH default is made explicit before bootstrap
so inherited search directories participate in the same decision. No authority
record, content identity, or caller grant changes.

## Private PID namespace switch and reported build mode (#3321, stage 1)

Design: `docs/design/private-pid-namespace.md`.

`SandboxPolicy::private_pid_namespace` is a staging switch, default false and
omitted from the serialized policy when false, so existing configuration and
serialized policies are unchanged. `LandlockSandbox::apply_rules` passes it to
fdguard's `build_isolation::confine`; it has no effect unless the child network
policy is `PrivateBuild`, and none on other backends. No production caller sets
it: with the switch on, a Brush worker reports its namespace-local PID and the
existing bootstrap check refuses it ("worker hello PID mismatch"), which is the
seam a later stage re-founds on kernel credentials. The serialized mechanism
tag stays `private-build`.

`BuildIsolation` (`Private` or `DenyAllFallback`) is new on `ConfinedChild` and
`SandboxedWorkerChild` as `build_isolation: Option<BuildIsolation>`. The spawn
funnel's shared fallback flag covers both preselected fallback and a retry after
private setup fails; the reported mode reads that flag after spawning. This is harness memory, not child output: a build can imitate
the diagnostic line on stderr but not this field. It is `None` unless the child
was spawned under `PrivateBuild` on Linux. `ManagedSpawn` and
`ConfinedTokioChild` do not carry it yet.

## Worker result-channel hardening (#3321)

`SandboxedWorker` supplies stdout and stderr with fresh Unix socketpairs on
Linux and macOS. The worker's stdout and stderr cannot be opened again by
pathname. A single parent write shutdown before spawn also gives the child's
unused read side EOF on the fresh pair. This avoids a redundant read shutdown
that can return ENOTCONN on macOS; errors from the required shutdown still
propagate. A Unix constructor test checks both forbidden directions, result
delivery and EOF. A sealed `ConfinedStdio::WorkerOutput` handle preserves
audited stdio credit without admitting arbitrary descriptors or providing a
host I/O service.

The parent endpoints remain in `Child.stdout` and `Child.stderr`, preserving
reader and supervisor ownership, including error paths. Ordinary argv stdio,
the authenticated stdin channel and the framing protocol are unchanged.
Retire with the upstream port of this transport change.

## Owned empty build scratch (#3361)

`ConfinedCommand` and `SandboxedWorker` carry caller-owned run paths separately
from environment and serialized policy. The Linux command hook forwards them to
fdguard for exact empty-directory recreation; all other backends retain their
existing behavior. See the fdguard patch notes for ownership and fallback rules.
Retire with the upstream port of this trusted scratch seam.

## Private PID startup report and teardown (#3321)

The shared spawn funnel receives one complete creator report and two pidfds, verifies relay pid/uid/gid kernel credentials, requires EOF, rejects malformed/truncated/extra data, and checks every exposed live PID/namespace-role fact. Reaped pidfds are accepted on authenticated creator-channel provenance: Linux discards their task metadata. BuildNamespace owns both handles across synchronous, managed and Tokio child conversions; bounded wait_gone reports incomplete teardown on expiry. Drop terminates through the init pidfd with a 500 ms bound, without repeating an already spent wait budget. The report contains foreign kernel coordinates, not a minted content identity.

Managed execution retires the reaped leader's numeric signalling authority before its bounded namespace wait. The existing retirement transition takes an injected teardown callback; pure regressions prove that waiting cannot keep the old process-group number signalable. Private executions also forget that number before waiting, disabling forced numeric group cleanup; fallback executions retain their existing group cleanup.
