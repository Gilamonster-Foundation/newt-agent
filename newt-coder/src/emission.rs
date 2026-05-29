//! Emission parser — stubbed in commit 1, real implementation in commit 3.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Emission {
    WholeFiles(BTreeMap<String, String>),
    UnifiedDiff(String),
    Prose(String),
}

impl Emission {
    pub fn shape_label(&self) -> &'static str {
        match self {
            Self::WholeFiles(_) => plugins_protocol::emission_shape::WHOLE_FILES,
            Self::UnifiedDiff(_) => plugins_protocol::emission_shape::UNIFIED_DIFF,
            Self::Prose(_) => plugins_protocol::emission_shape::PROSE,
        }
    }
}

pub fn normalize_emission(raw: &str) -> Result<Emission> {
    // Placeholder — full implementation lands in commit 3.
    Ok(Emission::Prose(raw.to_string()))
}
