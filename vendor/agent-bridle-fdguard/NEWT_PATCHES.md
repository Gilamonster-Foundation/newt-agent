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
internet sockets on stdio refuse execution. AF_UNIX remains available for the
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
and cannot write either host temporary root. The actual child cwd is checked
before setup, so a workspace covered by either mount selects fallback rather
than importing host contents. The diagnostic reports the effective combined mode.

`HOME`, `TMPDIR`, `TMP`, `TEMP`, and `XDG_RUNTIME_DIR` are not rewritten. Any of
their directories covered by the new mounts is recreated empty; no host files
or bind mounts are imported. Data pages are allocated on demand and charged to
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
Terminal stdio refuses exec; the existing descriptor sweep closes higher fds.

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
