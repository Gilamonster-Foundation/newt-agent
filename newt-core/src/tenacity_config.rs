//! Versioned pursuit defaults; the initiative module remains the family owner.
use super::Tenacity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current pursuit settings. Version 2 distinguishes these labels from the
/// pre-split tenacity table, which the one-time importer moves to initiative.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireConfig", into = "WireConfig")]
pub struct TenacityConfig {
    pub default: Option<Tenacity>,
    pub families: BTreeMap<String, Tenacity>,
}

#[derive(Serialize, Deserialize)]
struct WireConfig {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    default: Option<Tenacity>,
    #[serde(default)]
    families: BTreeMap<String, Tenacity>,
}

impl TryFrom<WireConfig> for TenacityConfig {
    type Error = String;

    fn try_from(wire: WireConfig) -> Result<Self, Self::Error> {
        match wire.version {
            Some(2) => Ok(Self { default: wire.default, families: wire.families }),
            None => Err("current [tenacity] settings require version = 2; unversioned legacy labels belong to the one-time initiative importer".to_string()),
            Some(version) => Err(format!("unsupported [tenacity] version {version}; supported version is 2")),
        }
    }
}

impl From<TenacityConfig> for WireConfig {
    fn from(config: TenacityConfig) -> Self {
        Self {
            version: Some(2),
            default: config.default,
            families: config.families,
        }
    }
}

impl TenacityConfig {
    /// Exact typed family label, case-insensitive; no model-name inference.
    pub fn family_default(&self, family: Option<&str>) -> Option<Tenacity> {
        let family = family?.trim();
        self.families
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(family))
            .map(|(_, level)| *level)
    }

    pub fn resolve(&self, family: Option<&str>) -> Tenacity {
        self.family_default(family)
            .or(self.default)
            .unwrap_or_default()
    }
}
