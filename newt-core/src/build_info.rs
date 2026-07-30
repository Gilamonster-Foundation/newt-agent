//! Compile-time identity for the exact Newt build.
//!
//! Cargo supplies the package version; `build.rs` adds the checked-out Git
//! commit and marks builds made from a modified worktree as `dirty`.

/// SemVer package version from the workspace manifest.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Twelve-character Git commit captured when `newt-core` was built.
pub const GIT_COMMIT: &str = env!("NEWT_BUILD_GIT_COMMIT");

/// Git commit plus a `-dirty` suffix when tracked or untracked changes existed.
pub const SOURCE_ID: &str = env!("NEWT_BUILD_SOURCE_ID");

/// User-visible build identity, for example `0.7.5 (251c70c1adb4)`.
pub const VERSION_WITH_COMMIT: &str = env!("NEWT_BUILD_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_version_contains_package_and_source_identity() {
        assert!(VERSION_WITH_COMMIT.starts_with(PACKAGE_VERSION));
        assert!(VERSION_WITH_COMMIT.contains(GIT_COMMIT));
        assert!(SOURCE_ID.starts_with(GIT_COMMIT));
    }
}
