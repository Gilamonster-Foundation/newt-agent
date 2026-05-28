//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod config;
pub mod error;
pub mod model_id;
pub mod router;
pub mod session;

pub use config::Config;
pub use error::NewtError;
pub use model_id::ModelId;
pub use router::{Router, Tier};
pub use session::SessionId;
