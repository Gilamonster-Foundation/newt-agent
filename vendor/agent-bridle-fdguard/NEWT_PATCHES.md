# Temporary Newt descriptor mechanism patch

Base: exact crates.io `agent-bridle-fdguard` 0.8.0-rc.5; original metadata and
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
