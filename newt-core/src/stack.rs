//! Thread stack policy shared across the newt binaries.
//!
//! The number lives here rather than in `newt-cli` because two crates need the
//! same floor and neither should depend on the other for it: `newt-cli` sizes
//! the CLI parse/dispatch thread and the Tokio workers, and `newt-tui` sizes
//! the per-session thread the chat loop runs on.

/// Minimum stack for a thread that runs deep newt code paths.
///
/// PR #746 hit `STATUS_STACK_OVERFLOW (0xC00000FD)` on Windows parsing the clap
/// command tree on the default ~1 MB main-thread stack; #747 is the guard
/// against dropping back below it. 16 MiB gives comparable room to larger Unix
/// defaults plus headroom.
///
/// The session thread (#1669) needs the same floor for a different reason: the
/// chat loop's command dispatch is a single ~4,900-line `match` arm, and giving
/// it a default-sized stack would rediscover the same class of crash on
/// Windows — with the failure landing mid-session rather than at startup, where
/// it is far harder to attribute.

/// The floor #746/#747 established. Named separately from the value below so
/// the guard compares two distinct constants — a `SESSION_STACK_BYTES >=
/// SESSION_STACK_BYTES` check would be a tautology, which is exactly the kind
/// of assertion that passes forever while measuring nothing.
const WINDOWS_STACK_FLOOR_BYTES: usize = 16 * 1024 * 1024;

pub const SESSION_STACK_BYTES: usize = WINDOWS_STACK_FLOOR_BYTES;

// Compile-time guard, mirroring `newt-cli`'s: a future "let's trim it" change
// fails the build rather than rediscovering #746/#747 on Windows CI.
const _: () = assert!(SESSION_STACK_BYTES >= WINDOWS_STACK_FLOOR_BYTES);
