//! Published authority types and their shared dispatch-site adaptor.

pub use agent_mesh_protocol::caveats::{Caveats, CountBound, Scope};

/// Per-axis "permits this concrete item?" check.
///
/// `All` permits everything; `Only(s)` permits exactly the members of `s`.
/// Defined as a trait because the upstream `agent-mesh-protocol::Scope` ships
/// only the lattice algebra; this is the dispatch-site adaptor. Constructors
/// (`Scope::only`, `Scope::none`) are inherent on the upstream type — no
/// re-definition needed.
pub trait ScopeExt<T: Ord + Clone> {
    /// Does this scope authorize `item`?
    fn permits(&self, item: &T) -> bool;
}

impl<T: Ord + Clone> ScopeExt<T> for Scope<T> {
    fn permits(&self, item: &T) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(item),
        }
    }
}
