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

`build_network` prepares a syscall-only pre-exec hook: unshare user and network
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
