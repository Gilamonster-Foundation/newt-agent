# Temporary Newt descriptor mechanism patch

Base: exact crates.io `agent-bridle-fdguard` 0.8.0-rc.7; original metadata and
license notices are retained. This remains the existing isolated unsafe seam.

`duplicate_control_socket` duplicates an explicitly supplied non-stdio fd with
F_DUPFD_CLOEXEC, then validates the owned duplicate as a connected Unix stream.
It neither takes ownership of the original nor opens a descriptor filesystem
path. Protocol authentication and transaction policy remain in the caller.
Existing ambient-descriptor closing behavior is unchanged.

Linux/macOS only. Windows needs the corresponding DuplicateHandle mechanism
and explicit HANDLE_LIST delegation with its separate authenticated transport.
Upstream the mechanism and remove this vendor override after publication and
platform-specific verification; this note does not claim that work completed.

## Build network setup (#3297)

`build_isolation` (formerly `build_network`) prepares a syscall-only pre-exec hook: unshare user and network
namespaces together, map the original uid/gid, enable only loopback, then apply
prebuilt Landlock and seccomp rules. All allocation precedes fork. A setup error
signals the existing spawn funnel to discard the child and retry once with the
original deny-all rules; no build code runs in a partial namespace. Rule-install
errors refuse execution. The ambient fd guard still closes inherited descriptors
at exec. The mode diagnostic is emitted only after the fence is installed.

Setup order is stdio socket validation, combined user/network unshare, uid/gid
mapping, loopback-up, seccomp (including no-new-privileges), then Landlock.
Inherited descriptors >=3 are marked CLOEXEC by the existing earlier hook;
host terminals and non-AF_UNIX sockets on stdio refuse execution in both private
and fallback modes. AF_UNIX remains available for the
authenticated worker channel and local build IPC. Namespace capabilities can
configure only owned network namespaces: raw IP sockets or new virtual devices
do not provide a host interface or route. AF_PACKET and io_uring stay denied.
The normal nonzero uid loses its namespace capabilities on exec; nested user
namespace creation remains permitted without removing the inherited fences.
Pathname Unix socket deputies remain outside this patch's direct-egress proof.

## Private temporary mounts (#3314)

The same pre-exec hook also unshares the mount namespace and makes every mount
recursively private before mounting tmpfs: `/tmp` has a 2 GiB data limit and
65,536 inodes; `/dev/shm` has 256 MiB and 16,384 inodes. Both use mode 0700,
nosuid and nodev; execution remains available to build/test artifacts. All
allocation precedes fork. The existing `linux-raw-sys` dependency in the graph
supplies the Landlock ABI layout, rather than duplicating it locally.

Each private rule opens its directory after mounting. A failed mount or rule
binding discards the entire child; the fresh fallback keeps its untouched rules
and adds no host temporary authority. Existing explicit grants still apply.
Before fork, covered request paths select that fallback without attempting
namespace setup. The diagnostic reports the effective combined mode.

`HOME`, `TMPDIR`, `TMP`, `TEMP`, and `XDG_RUNTIME_DIR` are not rewritten. A
covered directory selects fallback; no host files or directories are imported.
Data pages are allocated on demand and charged to
the allocating build's existing memory cgroup; namespaces do not supply a new
memory budget. Pages may use swap. Filesystem byte/inode bounds are per mount,
not an aggregate memory limit across builds. Final namespace/mount-reference
release frees the temporary filesystems. The child has no capabilities in host
namespaces and cannot mount after the Landlock fence.

## Private build terminals (#3320)

The same pre-fence mount namespace mounts devpts with
`newinstance,ptmxmode=0666,mode=0600,max=64`, nosuid and noexec. Binding its `ptmx`
onto `/dev/ptmx` supports both a host device node and a host symlink. The grants
bind the private directory and allocator inode after mounting; no host terminal
is granted. They contain only read/write, directory traversal, and IOCTL_DEV
when the prepared filesystem mask handles that right (Landlock ABI 5+).
The default ABI 3 does not restrict device ioctls. The shared core seccomp filter
denies TIOCSTI and TIOCLINUX while preserving allocation, termios and winsize.
Terminal stdio refuses exec in both private and fallback modes; the existing
descriptor sweep closes higher fds.

The 64-pair limit bounds one build instance, still subject to the kernel's global
PTY pool and reserve. It supplies no aggregate budget across concurrent builds.
The instance disappears when its last mount/namespace references are released.
Any setup failure discards the partially configured child and retries with the
untouched fallback ruleset, where PTY allocation remains denied. The real build
probe forces failure specifically at devpts mounting, after private scratch
grants, and exercises argv and Brush routes, host canaries, terminal stdio,
allocation/transport/settings, forbidden injection and the pair limit.

Kernel contracts: [devpts](https://docs.kernel.org/filesystems/devpts.html) and
[Landlock device ioctls](https://docs.kernel.org/userspace-api/landlock.html#device-ioctl).

## Private account files (#3319)

Before fork, the harness resolves its effective uid and primary gid through
reentrant NSS lookups, ignoring `HOME`, `USER` and `LOGNAME`. A bounded lookup
failure, missing entry, mismatched primary gid or malformed field selects the
existing fresh-child fallback. No NSS call or allocation runs in pre-exec.

After private scratch and terminal setup, the child writes exactly one passwd
record and one primary-group record on its private tmpfs and binds them over `/etc/passwd`
and `/etc/group`. Both mounts are readonly, nosuid, nodev and noexec. The
writable source aliases are unlinked and their descriptors closed before exec.
Only READ_FILE is granted, using descriptors opened on the mounted objects;
the fallback ruleset receives no account grant. Any setup failure discards the
child before build code runs and retries with the original deny-all floor.

The additional disclosure is the invoking account's name, numeric uid and gid,
home, shell and primary-group name. Password fields contain `x`; GECOS and
group membership lists are empty. No host account database, password hashes,
systemd userdb socket or sssd access is granted. The private namespace owns
the files' lifetime; no persistent identity format or new content id is minted.

The libc/NSS built-in root and nobody entries remain visible. They are constants
synthesized without reading host account data: their presence and fields reveal
neither whether those accounts exist on the host nor their host attributes.
The NSS dispatch configuration is unchanged; no private nsswitch view is needed.

The real-resource probe retains exactly one record in each private file and
checks normal own-user/group lookups. Before confinement it selects another real
host account by uid, excluding the caller and built-in uids, and proves that
lookup succeeds. The same lookup must fail inside each build route. It also
forces failure at the account's readonly remount, after scratch and terminal
setup, to prove the fresh fallback retains the original denials on argv and Brush.

## Same-object file binding (#3318)

`confine` takes prepared `SameObjectBind` values: a source and a target, each a
canonical absolute path with the regular file the caller verified and holds
open. After the private mounts and account files, and before seccomp and
Landlock, the hook reopens
each path in the new mount namespace with `openat2(RESOLVE_NO_SYMLINKS)` and
accepts it only if its device, inode and regular-file type equal the held
descriptor's. Inherited descriptors sit on the host's mounts and can be neither
cloned nor covered in the new namespace, so the reopen is required. The source
is cloned detached with `open_tree`, restricted with `mount_setattr` (read-only,
nosuid, nodev) and attached over the target with a descriptor-targeted
`move_mount`. The target path is then reopened and must be the held source.

Only a file is covered, never a directory. Any failure is a setup error: the
child exits before exec and the existing fresh-child fallback runs under the
original deny-all rules, with no namespace and therefore no binding. No path,
ownership fact or digest is passed through the environment, a file or a
message; descendants inherit the mount. Every descriptor is owned and closes on
every path, and the held pair is close-on-exec. `beneath`'s `openat2` wrapper is
shared crate-internally instead of duplicated.

Measured on Linux 6.8 with Landlock ABI 4: after the filesystem fence, `mount`,
`umount2` and `move_mount` on the target return EPERM, also from a later user
and mount namespace. `mount_setattr` is not mediated by Landlock; it needs
`CAP_SYS_ADMIN` over the mount namespace, which a nonzero uid loses at exec, and
it cannot change which object the target names. The binding's guarantee is that
identity, not the read-only flag: the file's own mode and the build's write
rules already refuse writes.

## Covered build inputs select fallback before fork (#3337)

The common hook chooses fallback before the first fork when the working directory,
any read/write grant root, PATH entry, tool home or temporary-directory environment
value is covered by `/tmp` or `/dev/shm`. Resolution precedes comparison, including
symlink targets and relative paths. A path that does not exist is judged by its
nearest existing ancestor, so a stale PATH entry or an absent grant root does not
cost the build its private mode; a missing working directory, any other resolution
failure and `..` select fallback conservatively. Known account-lookup failures
also select fallback before setup.
Brush carries these directory values, including its default PATH, through the
existing bootstrap seam, excluding loader and worker-authentication controls.

Fallback uses the original deny-all network and filesystem rules with the host
mount view. An explicitly granted temporary write remains on the host; an
ungranted write remains denied. No persisted record or identity changes.

### Owned empty run scratch (#3361)

The trusted caller now passes its acquired run directories separately from the
child environment through `ConfinedCommand` and `SandboxedWorker`. Only that
exact, existing, canonical directory is exempt as a grant root or temporary
HOME/TMPDIR/TMP/TEMP/XDG_RUNTIME_DIR value. PATH, GOPATH and tool-home inputs,
workspaces, ancestors, neighbors, descendants and symlink aliases retain the
covered-input decision, including nearest-existing-ancestor resolution.

Before fork, the hook prepares the owned scratch directory and its ancestors as
C strings; after mounting, `private_filesystems` recreates them empty with mode
0700, as before #3352. It neither copies nor binds host scratch contents. Newt's
Build requests expose one stable approved partition, including to pre-push,
permission ceilings and remembered grants. After acquiring scratch, the argv
executor and Brush dispatch narrow only scratch-contributed roots to that run.
Independent input roots remain intact, even at the same partition. Approval
checks retain the original exact lattice comparison; the spawn projection never
changes or enters cached approval authority.

An operator override outside the covered mounts stays host-backed. An override
inside them is still an owned empty run and uses the same private recreation.
This is explicit caller provenance, never a naming convention or child input.
The host uses scratch only for ownership/cleanup: build artifacts are in the
workspace target, toolchains are admitted from held operator roots before spawn,
and Brush results/stdout/stderr use socketpairs. The opt-in PID relay/init use
pipes and wait status, not scratch files. No host consumer reads private outputs
from the recreated scratch. Real probes require default-scratch private mode on
both routes and prove that unrelated and scratch-neighbor grants retain host
writes in fallback. The mocked decision table exercises the exact exception.

## Private PID namespace topology, opt-in (#3321, stage 1)

Design: `docs/design/private-pid-namespace.md`. `confine` takes one more
argument, `pid_namespace`. When false (every production caller today) the hook
is unchanged. When true, after the account files and verified Git alias bindings are in place and before either
fence, the hook calls `unshare(CLONE_NEWPID)` as a separate call and creates two
processes with raw `clone3`:

- **init** (PID 1): never execs. It sets `PR_SET_PDEATHSIG`, becomes
  non-dumpable, resets every signal to its default and ignores `SIGCHLD` so the
  kernel reaps adopted orphans, installs the deny seccomp filter and the
  fallback Landlock ruleset, drops all capabilities, closes every descriptor
  except a lifeline read end and a ready write end, writes one ready byte, and
  blocks reading the lifeline. End-of-file means the relay is gone.
- **payload** (PID 2, no visible parent): leads its own process group when the
  hook's process did, then finishes the existing hook and is the process std
  execs. Program, argv, environment and stdio are untouched.

The hook's own process stays in the host PID namespace as the **relay**. It
applies the same hardening, closes every descriptor except the lifeline write
end (which drops std's exec-status pipe, so `spawn` still returns at the
payload's exec), waits for the payload, kills a payload that reports a ptrace
stop, then closes the lifeline, kills and reaps init, and ends with the
payload's exit code or by the payload's signal. Reaping init returns only after
the kernel has killed every member, so a normal exit leaves nothing behind.
Killing the relay by any means ends init and therefore every member, including
one that changed session.

Any failure before the payload exists (the unshare, a pipe, either clone, init
not reporting ready) returns the existing retry signal with init already
killed and reaped; a failure in the payload returns it through std as before.
The fresh fallback child never unshares. If the relay cannot fence itself it
writes one line to stderr and exits 1, which ends the namespace.

Everything after fork is raw syscalls on stack storage: `clone3`, `prlimit64`,
`rt_sigaction`, `capset` and `close_range` use local kernel-layout structs
rather than libc wrappers, because musl's `setrlimit` synchronises threads and
its `sigaction` takes a lock for `SIGABRT`. An `strace -f` of the real probe
shows the relay and init issuing only `unshare`, `pipe2`, `clone3`, `close`,
`read`, `write`, `prctl`, `prlimit64`, `rt_sigaction`, `seccomp`,
`landlock_restrict_self`, `capset`, `close_range`, `wait4`, `kill` and
`exit_group`: no `mmap`, `brk` or `futex`.

Not in this stage: the private procfs mount and init's argv zeroing (stage 8),
host-side handles for init and the payload (stage 2). The relay and init are
copy-on-write images of the harness for the build's lifetime; they are
non-dumpable and hold no descriptors but the lifeline. The mocked tests cover
the relay's status decision and the `clone_args` layout; the real-resource
probe `--build-pid-probe` in `newt-core/tests/brush_build_pipeline.rs` grounds
them on both spawn funnels, with fault injection at the PID unshare and at
init's hardening.

## Shared leaf and retained tests

Newt core and MCP now use this crate directly, including without their `confine`
feature; the separate unpublished copy is removed. The former differential
suite lives in `tests/bridle_differential.rs`: literal errno/identity assertions,
independent filesystem observations, and a guarded-versus-unguarded child probe.
The upstream hostile-descriptor suite is retained here without a second copy.
Linux `build_isolation` remains unchanged: its dependency versions are already
in Newt's confine-off closure, so feature-gating it would remove no packages.
