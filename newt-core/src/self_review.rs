//! Independently selected review: shared subject/result validation.
//! Transport and global admission remain with the existing turn/scheduler hosts.
mod capture;
mod evidence;
mod git_capture;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod git_metadata;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod git_read;
mod phase;
mod subject;
mod tree;

pub use capture::capture_artifacts;
pub use git_capture::capture_existing_diff;
pub use phase::{ReviewInput, ReviewPhase, ReviewTransition};
pub use tree::{capture_workspace, WorkspaceSnapshot};

pub use evidence::{
    consume_evidence, record_reply, Assessment, ReviewEvent, ReviewFailure, ReviewStatus,
};
pub use subject::{
    CaptureFailure, ImplementationScope, PresentedSubject, ReviewCoverage, ReviewFile,
    ReviewObjective, ReviewSubject, ReviewVersions,
};

#[cfg(test)]
#[path = "self_review_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "self_review_capture_tests.rs"]
mod capture_tests;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "self_review_tree_tests.rs"]
mod tree_tests;

#[cfg(test)]
#[path = "self_review_git_tests.rs"]
mod git_tests;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "self_review_implementation_tests.rs"]
mod implementation_tests;

#[cfg(test)]
#[path = "self_review_read_limits_tests.rs"]
mod read_limits_tests;

#[cfg(test)]
#[path = "self_review_git_safety_tests.rs"]
mod git_safety_tests;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "self_review_root_identity_tests.rs"]
mod root_identity_tests;

#[cfg(test)]
#[path = "self_review_phase_tests.rs"]
mod phase_tests;
