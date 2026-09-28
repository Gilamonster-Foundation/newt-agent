//! Portable toolchain contracts, independent of a particular agent harness.

pub mod caveats;
pub mod native_git;

#[cfg(feature = "embedded-git")]
pub mod embedded_git;
#[cfg(feature = "embedded-git")]
pub mod git_caveats;
