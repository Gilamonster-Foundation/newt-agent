//! Newt-Agent core: shared types, errors, and the tier router.
//!
//! The router is the NeMoCode inheritance — it classifies an incoming turn
//! into a `Tier` (FAST / STANDARD / COMPLEX / REVIEW), and asks the
//! configured backends which can serve that tier.

pub mod caveats;
pub mod config;
pub mod dgx;
pub mod error;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod model_id;
pub mod pricing;
pub mod router;
pub mod session;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use agent_mesh_protocol::{Caveats, CountBound, Scope};
pub use caveats::{CaveatsExt, CountBoundExt, ScopeExt};
pub use config::{
    BackendConfig, BackendKind, ChatStyle, Config, EditMode, LogConfig, MemoryConfig,
    MemoryProviderKind, PermissionPreset, ToolPermissions, TuiConfig,
};
pub use dgx::{DgxConfig, DgxFormation, DgxNode, DgxNotConfigured, EndpointKind};
pub use error::NewtError;
pub use memory::{
    MemMessage, MemoryManager, MemoryProvider, NoteStore, Role, RollingWindow, SessionContext,
    SoulProvider, SoulSource, Summarizing, TokenBudget, DEFAULT_SOUL,
};
pub use metrics::{TokenUsage, TurnMetrics};
pub use model_id::ModelId;
pub use pricing::{ModelRate, PricingConfig};
pub use router::{Router, Tier};
pub use session::SessionId;
