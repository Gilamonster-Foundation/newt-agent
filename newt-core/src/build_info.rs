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

/// Compiled-in default harness/brand name — the GitHub User
/// [`newt-agent`](https://github.com/newt-agent), overridden by `NEWT_BRAND_NAME`
/// for a downstream rebrand (e.g. `gilamonster-agent`).
pub const DEFAULT_BRAND_NAME: &str = "newt-agent";

/// The execution harness identity a live contribution is attributed under
/// (`NEWT_BRAND_NAME` env override, else [`DEFAULT_BRAND_NAME`]).
///
/// The single authoritative source for "which harness is this" — previously
/// duplicated as a private `brand_name()` in `newt-git` (the git-attribution
/// footer) independently of `newt-tui`'s UI-wordmark `brand_name()` (which
/// stays separate on purpose: a different default, `"newt"`, for a different
/// purpose, the splash/prompt wordmark).
#[must_use]
pub fn harness_name() -> String {
    std::env::var("NEWT_BRAND_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BRAND_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn build_version_contains_package_and_source_identity() {
        assert!(VERSION_WITH_COMMIT.starts_with(PACKAGE_VERSION));
        assert!(VERSION_WITH_COMMIT.contains(GIT_COMMIT));
        assert!(SOURCE_ID.starts_with(GIT_COMMIT));
    }

    #[test]
    #[serial(newt_brand_name_env)]
    fn harness_name_defaults_to_newt_agent() {
        // SAFETY: serialized against other NEWT_BRAND_NAME-mutating tests.
        unsafe { std::env::remove_var("NEWT_BRAND_NAME") };
        assert_eq!(harness_name(), "newt-agent");
    }

    #[test]
    #[serial(newt_brand_name_env)]
    fn harness_name_honors_rebrand_override() {
        // SAFETY: serialized against other NEWT_BRAND_NAME-mutating tests.
        unsafe { std::env::set_var("NEWT_BRAND_NAME", "gilamonster-agent") };
        assert_eq!(harness_name(), "gilamonster-agent");
        unsafe { std::env::remove_var("NEWT_BRAND_NAME") };
    }

    #[test]
    #[serial(newt_brand_name_env)]
    fn harness_name_ignores_blank_override() {
        // SAFETY: serialized against other NEWT_BRAND_NAME-mutating tests.
        unsafe { std::env::set_var("NEWT_BRAND_NAME", "   ") };
        assert_eq!(harness_name(), "newt-agent");
        unsafe { std::env::remove_var("NEWT_BRAND_NAME") };
    }
}
