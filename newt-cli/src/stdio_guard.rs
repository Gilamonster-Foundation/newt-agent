//! Stdio safety guard for headless JSON-RPC servers.
//!
//! The `newt worker` and `newt mcp` subcommands use **stdout as the
//! JSON-RPC wire**. Any rogue `println!()` from a dependency would
//! interleave with protocol frames and corrupt the wire. Tracing is
//! already routed to stderr (see the binary `main.rs` files), but
//! `println!` and direct writes to fd 1 are harder to police — we
//! can't statically forbid them across the dep tree.
//!
//! [`redirect_stdout_to_stderr`] solves this structurally on Unix by:
//!
//! 1. `dup(1)` — clone the real stdout file descriptor.
//! 2. `dup2(2, 1)` — redirect fd 1 to where fd 2 (stderr) points.
//!
//! After this, `println!` (and any other write to fd 1) lands on
//! stderr. The returned [`std::fs::File`] is the private copy of the
//! real stdout — hand it to the JSON-RPC server as the writer.
//!
//! On non-Unix platforms this module is a no-op fallback that just
//! returns `std::io::stdout()` wrapped in a file-like interface
//! preserved via stdout; the foot-gun is documented as out-of-scope
//! there.

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;

/// Redirect process stdout to stderr and return a private file handle
/// pointing at the original stdout.
///
/// After calling this, anything that writes to fd 1 (the `println!`
/// macro, a dependency's stray `write!`, etc.) ends up on stderr.
/// The returned [`File`] is the **only** way to write to the real
/// stdout — the JSON-RPC server must use it as its writer.
///
/// # Safety
///
/// The `dup`/`dup2` calls are technically `unsafe` but the contract
/// is straightforward: we own the resulting raw fd, we wrap it in a
/// `File` so `Drop` closes it, and we never reuse the original fd 1
/// after redirection. Failure of either syscall returns an error
/// instead of panicking — the caller can fall back to plain stdout.
///
/// # Errors
///
/// Returns [`std::io::Error`] if either `dup` or `dup2` fails.
#[cfg(unix)]
pub fn redirect_stdout_to_stderr() -> std::io::Result<File> {
    // Safety: dup(1) returns a new fd owned by us. We immediately
    // wrap it in a File so Drop will close it if anything below fails.
    let cloned_fd = unsafe { libc::dup(1) };
    if cloned_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Safety: cloned_fd is a valid, owned fd we just received from dup.
    let private_stdout = unsafe { File::from_raw_fd(cloned_fd) };

    // Safety: dup2(2, 1) atomically closes fd 1 (the public stdout)
    // and makes it an alias for fd 2 (stderr). Any subsequent writes
    // to fd 1 land on stderr.
    let rc = unsafe { libc::dup2(2, 1) };
    if rc < 0 {
        // private_stdout's Drop closes the cloned fd cleanly.
        return Err(std::io::Error::last_os_error());
    }

    Ok(private_stdout)
}

/// Non-Unix fallback: no redirection. Returns `None` and callers
/// should use plain stdout. The stdout-corruption foot-gun is
/// documented as out-of-scope on Windows for now — see the PR body.
#[cfg(not(unix))]
pub fn redirect_stdout_to_stderr() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stdout fd-redirect not implemented on non-Unix targets; \
         use plain stdout and audit dep tree for stray println!",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    //! The interesting test is the cross-process one in
    //! `tests/stdout_purity.rs` — calling `redirect_stdout_to_stderr`
    //! inside the test process itself would clobber the test
    //! harness's own stdout (and break `cargo test` reporting).
    //!
    //! Here we only test that the function returns a usable File
    //! handle without panicking when the syscalls succeed. To do that
    //! we'd need to actually call dup/dup2, which we can't do
    //! reliably inside a test process — so the real coverage lives in
    //! the subprocess test.

    #[test]
    fn module_compiles_on_unix() {
        // Smoke test: the function exists and has the right signature.
        let _f: fn() -> std::io::Result<std::fs::File> = super::redirect_stdout_to_stderr;
    }
}
