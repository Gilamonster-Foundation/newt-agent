//! Stack policy for the `newt` CLI entrypoint.
//!
//! Windows starts the process main thread with a much smaller stack than Linux.
//! As the clap command tree grows, `Cli::parse()` can overflow that stack before
//! the binary emits any diagnostics. Keep parse + dispatch behind this explicit
//! stack policy so future CLI growth does not rediscover the same Windows-only
//! crash.

/// Minimum stack size needed for the expanded clap command tree on Windows.
const WINDOWS_CLI_PARSE_STACK_FLOOR_BYTES: usize = 16 * 1024 * 1024;

/// Stack used for the CLI parse/dispatch thread and Tokio worker threads.
///
/// PR #746 hit `STATUS_STACK_OVERFLOW (0xC00000FD)` on Windows while parsing the
/// clap command tree on the default ~1 MB main-thread stack. 16 MiB gives clap
/// room comparable to larger Unix defaults plus headroom for future subcommands.
pub const CLI_THREAD_STACK_BYTES: usize = WINDOWS_CLI_PARSE_STACK_FLOOR_BYTES;

// Compile-time guard: dropping below this floor reopens #747.
const _: () = assert!(CLI_THREAD_STACK_BYTES >= WINDOWS_CLI_PARSE_STACK_FLOOR_BYTES);

/// Run `f` on a thread with Newt's explicit CLI stack policy.
///
/// The return value is forwarded. If the worker panics, resume that panic on the
/// caller so tests and process failures keep their original panic payload.
pub fn run_on_cli_stack<F, T>(name: &'static str, f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(CLI_THREAD_STACK_BYTES)
        .spawn(f)
        .unwrap_or_else(|e| {
            panic!(
                "spawn {name} thread with {} MiB stack: {e}",
                CLI_THREAD_STACK_BYTES / 1024 / 1024
            )
        })
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

/// Build the Tokio runtime used by the CLI entrypoint.
///
/// Worker threads get the same explicit stack as the parse/dispatch thread so
/// deep CLI-adjacent async paths do not silently fall back to a smaller default.
pub fn build_cli_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(CLI_THREAD_STACK_BYTES)
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_on_cli_stack_returns_the_worker_value() {
        let got = run_on_cli_stack("newt-stack-test", || 42);
        assert_eq!(got, 42);
    }
}
