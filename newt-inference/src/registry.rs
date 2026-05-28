use std::sync::Arc;

use newt_core::router::Tier;
use newt_core::NewtError;

use crate::backend::InferenceBackend;

/// Registry of inference backends, ordered by config preference.
pub struct BackendRegistry {
    entries: Vec<Arc<dyn InferenceBackend>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn register(&mut self, backend: Arc<dyn InferenceBackend>) {
        self.entries.push(backend);
    }

    /// Pick the first registered backend that supports the given tier.
    pub fn pick(&self, tier: Tier) -> Result<Arc<dyn InferenceBackend>, NewtError> {
        self.entries
            .iter()
            .find(|b| b.supports_tier(tier))
            .cloned()
            .ok_or(NewtError::NoBackendForTier(tier))
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|b| b.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}
