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
