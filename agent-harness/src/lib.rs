//! Accounted context projection and bounded host policy, independent of inference.
#![forbid(unsafe_code)]

pub mod forensics;
pub mod navigation;
pub mod projection;
pub mod render;
pub mod session;
pub mod store;
pub use agent_frame::ReplyVerdict as Verdict;
pub use forensics::replay_from_store;
pub use session::{PreparedRequest, Session, SessionConfig};

/// A refused host operation. Errors are surfaced; none imply model completion.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("frame storage: {0}")]
    Storage(String),
    #[error("run writer conflict: {0}")]
    Conflict(String),
    #[error("frame integrity: {0}")]
    Integrity(String),
    #[error("invalid proposal: {0}")]
    Proposal(String),
    #[error("harness budget exhausted: {0}")]
    Budget(String),
    #[error("frame access denied: {0}")]
    Access(String),
}

pub type Result<T> = std::result::Result<T, Error>;
