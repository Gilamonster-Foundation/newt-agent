//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod error;
pub mod router;

pub use error::NewtError;
pub use router::{Router, Tier};
