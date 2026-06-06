//! Community model tuning profiles.
//!
//! Defines the shareable TOML format for model tuning data and provides
//! load/save helpers for `~/.newt/community-tunings.toml`.
//!
//! The community format is deliberately independent of the newt codebase —
//! it can be shared as a gist, forum post, or repository without any code
//! dependency. Import with `newt tunings import <file>`.
//!
//! # File format
//!
//! ```toml
//! # newt community model tuning profiles · format v1
//! [format]
//! version = "1"
//! generated_by = "newt/0.6.6"
//!
//! [[profiles]]
//! model = "nemotron3:33b"
//! context_window = 32768
//! safe_context = 24576
//! mid_loop_trim_threshold = 12
//! max_tool_rounds = 20
//! tune_source = "empirical"
//! confidence = "high"
//! data_points = 15
//! notes = "Overflows at ~30k tokens; 24k confirmed safe for 10-round tool sessions"
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// How the tuning parameters were derived.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuneSource {
    /// Measured from real inference sessions by the auto-tuner.
    #[default]
    Empirical,
    /// Taken from the model's declared metadata via Ollama `/api/show`.
    OllamaShow,
    /// Set manually by the user.
    Manual,
    /// Contributed by the community without local validation.
    Community,
}

/// One model's tuning profile in the community format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningProfile {
    /// Model name as it appears in Ollama (e.g. `"nemotron3:33b"`).
    pub model: String,

    /// Declared context window from Ollama `/api/show` (tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,

    /// Recommended `num_ctx` to send to Ollama — safe for multi-tool sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_context: Option<u32>,

    /// Recommended `mid_loop_trim_threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_threshold: Option<usize>,

    /// Recommended `max_tool_rounds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<usize>,

    /// How these values were derived.
    #[serde(default)]
    pub tune_source: TuneSource,

    /// Confidence level: "none", "low", "medium", or "high".
    #[serde(default = "default_confidence")]
    pub confidence: String,

    /// Number of inference sessions that informed this profile.
    #[serde(default)]
    pub data_points: u32,

    /// Free-text notes for human readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

fn default_confidence() -> String {
    "none".to_string()
}

/// File-level metadata for the community tunings format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatMeta {
    /// Format version — currently always `"1"`.
    pub version: String,
    /// `"newt/<semver>"` string identifying the exporter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
    /// ISO-8601 timestamp of export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

impl Default for FormatMeta {
    fn default() -> Self {
        Self {
            version: "1".to_string(),
            generated_by: None,
            generated_at: None,
        }
    }
}

/// Top-level community tunings document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommunityTunings {
    #[serde(default)]
    pub format: FormatMeta,
    /// Model tuning profiles.
    #[serde(default)]
    pub profiles: Vec<TuningProfile>,
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Path to the community tunings file: `~/.newt/community-tunings.toml`.
pub fn community_tunings_path() -> Option<PathBuf> {
    crate::config::Config::user_config_path().map(|p| p.with_file_name("community-tunings.toml"))
}

/// Load community tunings from `~/.newt/community-tunings.toml`.
/// Returns an empty document on any error (file absent, parse failure, etc.).
pub fn load_community_tunings() -> CommunityTunings {
    let Some(path) = community_tunings_path() else {
        return CommunityTunings::default();
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        return CommunityTunings::default();
    };
    toml::from_str(&data).unwrap_or_default()
}

/// Save community tunings to `~/.newt/community-tunings.toml` (best-effort).
pub fn save_community_tunings(tunings: &CommunityTunings) -> std::io::Result<()> {
    let path = community_tunings_path()
        .ok_or_else(|| std::io::Error::other("cannot determine ~/.newt path"))?;
    let data = toml::to_string_pretty(tunings).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, data)
}

/// Find a profile for the given model name (exact match).
impl CommunityTunings {
    pub fn find(&self, model: &str) -> Option<&TuningProfile> {
        self.profiles.iter().find(|p| p.model == model)
    }

    /// Merge `other` into `self`: existing profiles are updated if the
    /// incoming entry has higher confidence; new profiles are appended.
    pub fn merge(&mut self, other: Self) {
        for incoming in other.profiles {
            if let Some(existing) = self.profiles.iter_mut().find(|p| p.model == incoming.model) {
                if confidence_rank(&incoming.confidence) >= confidence_rank(&existing.confidence) {
                    *existing = incoming;
                }
            } else {
                self.profiles.push(incoming);
            }
        }
    }
}

fn confidence_rank(c: &str) -> u8 {
    match c {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_tunings_roundtrips_toml() {
        let mut ct = CommunityTunings::default();
        ct.format.version = "1".to_string();
        ct.format.generated_by = Some("newt/0.6.6".to_string());
        ct.profiles.push(TuningProfile {
            model: "nemotron3:33b".to_string(),
            context_window: Some(32768),
            safe_context: Some(24576),
            mid_loop_trim_threshold: Some(12),
            max_tool_rounds: Some(20),
            tune_source: TuneSource::Empirical,
            confidence: "high".to_string(),
            data_points: 15,
            notes: Some("confirmed safe".to_string()),
        });
        let serialized = toml::to_string_pretty(&ct).unwrap();
        let back: CommunityTunings = toml::from_str(&serialized).unwrap();
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].model, "nemotron3:33b");
        assert_eq!(back.profiles[0].safe_context, Some(24576));
        assert_eq!(back.profiles[0].confidence, "high");
    }

    #[test]
    fn community_tunings_find_returns_correct_profile() {
        let ct = CommunityTunings {
            profiles: vec![
                TuningProfile {
                    model: "a:7b".to_string(),
                    context_window: None,
                    safe_context: None,
                    mid_loop_trim_threshold: None,
                    max_tool_rounds: None,
                    tune_source: TuneSource::Manual,
                    confidence: "medium".to_string(),
                    data_points: 0,
                    notes: None,
                },
                TuningProfile {
                    model: "b:13b".to_string(),
                    context_window: Some(8192),
                    safe_context: Some(6553),
                    mid_loop_trim_threshold: None,
                    max_tool_rounds: None,
                    tune_source: TuneSource::OllamaShow,
                    confidence: "low".to_string(),
                    data_points: 1,
                    notes: None,
                },
            ],
            ..CommunityTunings::default()
        };
        assert!(ct.find("a:7b").is_some());
        assert_eq!(ct.find("b:13b").unwrap().context_window, Some(8192));
        assert!(ct.find("c:30b").is_none());
    }

    #[test]
    fn merge_replaces_lower_confidence_with_higher() {
        let mut base = CommunityTunings {
            profiles: vec![TuningProfile {
                model: "m:7b".to_string(),
                safe_context: Some(4096),
                confidence: "low".to_string(),
                context_window: None,
                mid_loop_trim_threshold: None,
                max_tool_rounds: None,
                tune_source: TuneSource::Community,
                data_points: 1,
                notes: None,
            }],
            ..CommunityTunings::default()
        };
        let incoming = CommunityTunings {
            profiles: vec![TuningProfile {
                model: "m:7b".to_string(),
                safe_context: Some(8192),
                confidence: "high".to_string(),
                context_window: None,
                mid_loop_trim_threshold: None,
                max_tool_rounds: None,
                tune_source: TuneSource::Empirical,
                data_points: 10,
                notes: None,
            }],
            ..CommunityTunings::default()
        };
        base.merge(incoming);
        assert_eq!(base.profiles.len(), 1);
        assert_eq!(base.profiles[0].safe_context, Some(8192));
        assert_eq!(base.profiles[0].data_points, 10);
    }

    #[test]
    fn merge_does_not_replace_higher_confidence_with_lower() {
        let mut base = CommunityTunings {
            profiles: vec![TuningProfile {
                model: "m:7b".to_string(),
                safe_context: Some(8192),
                confidence: "high".to_string(),
                context_window: None,
                mid_loop_trim_threshold: None,
                max_tool_rounds: None,
                tune_source: TuneSource::Empirical,
                data_points: 10,
                notes: None,
            }],
            ..CommunityTunings::default()
        };
        let incoming = CommunityTunings {
            profiles: vec![TuningProfile {
                model: "m:7b".to_string(),
                safe_context: Some(2048),
                confidence: "low".to_string(),
                context_window: None,
                mid_loop_trim_threshold: None,
                max_tool_rounds: None,
                tune_source: TuneSource::Community,
                data_points: 1,
                notes: None,
            }],
            ..CommunityTunings::default()
        };
        base.merge(incoming);
        // The existing high-confidence entry wins.
        assert_eq!(base.profiles[0].safe_context, Some(8192));
        assert_eq!(base.profiles[0].data_points, 10);
    }

    #[test]
    fn confidence_rank_orders_correctly() {
        assert!(confidence_rank("high") > confidence_rank("medium"));
        assert!(confidence_rank("medium") > confidence_rank("low"));
        assert!(confidence_rank("low") > confidence_rank("none"));
        assert_eq!(confidence_rank("none"), 0);
    }

    #[test]
    fn empty_document_parses_without_error() {
        let ct: CommunityTunings = toml::from_str("").unwrap();
        assert!(ct.profiles.is_empty());
    }
}
