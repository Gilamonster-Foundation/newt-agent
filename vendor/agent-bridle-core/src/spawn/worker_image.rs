//! Live worker-image handle; the path is declaration text, not an exec operand.
use std::fs::File;

use crate::{SandboxKind, ToolError, ToolResult};

pub(super) struct WorkerImage {
    pub(super) program: String,
    pub(super) file: File,
}

impl WorkerImage {
    /// Reuse the held-root resolver. The final open refuses symlinks and does
    /// no I/O; fstat must confirm a regular file before admission can add it.
    /// This pins the inode, not immutable bytes: in-place writes are outside
    /// this pathname-substitution guarantee.
    pub(super) fn bind(kind: SandboxKind, program: &str) -> ToolResult<Self> {
        #[cfg(target_os = "linux")]
        {
            if kind != SandboxKind::Landlock {
                return Err(ToolError::denied(
                    "worker backend cannot bind rules and execution to a held image",
                ));
            }
            let program = crate::admitted::canonical_closure_program(program)?;
            let path = std::path::Path::new(&program);
            let parent = path
                .parent()
                .ok_or_else(|| ToolError::denied("worker image has no parent"))?;
            let name = path
                .file_name()
                .ok_or_else(|| ToolError::denied("worker image has no filename"))?;
            let root = agent_bridle_fdguard::GrantedRoot::acquire(parent)
                .map_err(|e| ToolError::denied(format!("cannot bind worker parent: {e}")))?;
            // `name` is exactly one filename component. Reuse the held-root
            // resolver for ancestors; the final openat cannot walk or escape.
            // O_PATH does no I/O (including on FIFOs/devices), O_NOFOLLOW holds
            // a planted symlink itself, and the fstat below rejects it.
            let file = File::from(
                rustix::fs::openat(
                    root.as_fd(),
                    name,
                    rustix::fs::OFlags::PATH
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|e| ToolError::denied(format!("cannot bind worker image: {e}")))?,
            );
            if !file.metadata()?.is_file() {
                return Err(ToolError::denied(
                    "trusted worker image must be a regular file",
                ));
            }
            // Command's stdio setup may overwrite descriptors 0..=2. Duplicate
            // into the non-stdio range without reopening the object.
            let file =
                File::from(rustix::io::fcntl_dupfd_cloexec(&file, 3).map_err(|e| {
                    ToolError::denied(format!("cannot retain worker descriptor: {e}"))
                })?);
            Ok(Self { program, file })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (kind, program);
            Err(ToolError::denied("trusted worker image binding is unsupported on this platform; refusing pathname execution"))
        }
    }

    /// Linux resolves this magic link to the held descriptor at exec, even if
    /// the original pathname was replaced or removed. No pathname fallback.
    /// CLOEXEC closes the descriptor only AFTER successful exec; fdguard's
    /// CLOEXEC sweep preserves it through resolution. Missing procfs or an
    /// interpreter needing the descriptor after exec fails closed.
    pub(super) fn exec_operand(&self) -> ToolResult<String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            Ok(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ToolError::denied(
                "held worker execution is unsupported on this platform",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #419: fstat rejects a directory before it can become a read rule.
    #[cfg(target_os = "linux")]
    #[test]
    fn worker_binding_requires_a_regular_file() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let error = WorkerImage::bind(SandboxKind::Landlock, dir.to_str().unwrap())
            .err()
            .expect("directory must be refused");
        assert!(error.to_string().contains("regular file"), "{error}");
    }

    /// #419 round 2: non-Landlock backends use their own launch route, never
    /// this Linux binding primitive (including Noop on a Linux host).
    #[test]
    fn non_landlock_backends_cannot_enter_image_binding() {
        for kind in [
            SandboxKind::Seatbelt,
            SandboxKind::AppContainer,
            SandboxKind::None,
        ] {
            let error = WorkerImage::bind(kind, "unused-worker-path")
                .err()
                .expect("only Landlock may enter image binding");
            assert!(error.to_string().contains("bind"), "{kind:?}: {error}");
        }
    }
}
