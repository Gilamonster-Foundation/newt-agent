use serde::{Deserialize, Serialize};
use std::fmt;

/// Strongly-typed model identifier for audit trail purposes.
/// Every inference response must carry a ModelId so the arbiter and scorecard
/// can attribute the work to a specific model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for ModelId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_display() {
        let id = ModelId::new("llama3.1:8b");
        assert_eq!(id.to_string(), "llama3.1:8b");
    }

    #[test]
    fn as_str() {
        let id = ModelId::new("gpt-4o");
        assert_eq!(id.as_str(), "gpt-4o");
    }

    #[test]
    fn from_string() {
        let id: ModelId = "test-model".into();
        assert_eq!(id.as_str(), "test-model");
    }

    #[test]
    fn serde_roundtrip() {
        let id = ModelId::new("claude-3");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"claude-3\"");
        let back: ModelId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn equality() {
        assert_eq!(ModelId::new("a"), ModelId::new("a"));
        assert_ne!(ModelId::new("a"), ModelId::new("b"));
    }
}
