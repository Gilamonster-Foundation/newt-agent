use std::sync::Arc;

use newt_core::router::Tier;
use newt_core::NewtError;

use crate::backend::InferenceBackend;
use crate::provider_plugin::ProviderPluginBackend;

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

    pub fn load_from_config(cfg: &newt_core::Config) -> anyhow::Result<Self> {
        let mut registry = Self::new();
        registry.register_configured_providers(cfg)?;
        Ok(registry)
    }

    pub fn register_configured_providers(
        &mut self,
        cfg: &newt_core::Config,
    ) -> anyhow::Result<usize> {
        let start_len = self.entries.len();
        for provider in &cfg.providers {
            self.register(Arc::new(ProviderPluginBackend::from_config(provider)?));
        }
        Ok(self.entries.len() - start_len)
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::config::ProviderConfig;
    use newt_core::{Config, Tier};

    fn provider(model: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "openai".into(),
            command: "newt-provider-openai".into(),
            model: model.map(str::to_string),
            env_pass: vec!["OPENAI_API_KEY".into()],
            tiers: vec![Tier::Complex],
        }
    }

    #[test]
    fn load_from_config_registers_provider_plugins_in_order() {
        let cfg = Config {
            providers: vec![provider(Some("gpt-test"))],
            ..Config::default()
        };

        let registry = BackendRegistry::load_from_config(&cfg).unwrap();

        assert_eq!(registry.names(), vec!["openai"]);
        assert_eq!(registry.pick(Tier::Complex).unwrap().model_id(), "gpt-test");
    }

    #[test]
    fn load_from_config_rejects_provider_without_model() {
        let cfg = Config {
            providers: vec![provider(None)],
            ..Config::default()
        };

        let err = BackendRegistry::load_from_config(&cfg)
            .err()
            .expect("missing provider model should fail");

        assert!(err.to_string().contains("missing required model"));
    }
}
