//! Pure-data probe rules for `newt mcp probe` (#1292).
//!
//! When the operator names a command but no `--arg`, the probe tries a short,
//! ordered list of conventional arg spellings (`["stdio"]`, none, `["mcp"]`,
//! `["serve"]`). That list is domain knowledge, so per the three Cs it lives
//! in DATA: a bundled `mcp_probe/default.toml` compiled into the binary,
//! wholesale-replaceable by a `mcp-probe-rules.toml` drop-in (project
//! `.newt/` wins over `~/.newt/`). Replacement — not merge — because this is
//! an *ordered trial list*, not a keyed set. This module is fully pure; the
//! CLI owns the file reads.

use serde::Deserialize;

use crate::error::{NewtError, Result};

const BUNDLED_RULES: &str = include_str!("mcp_probe/default.toml");

/// The probe's trial rules. `deny_unknown_fields` keeps a typo'd drop-in a
/// loud error, never a silently empty rule set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeRules {
    /// Arg spellings to try, in order, when `--arg` was not given. Each inner
    /// list is one spawn candidate's full argv tail.
    #[serde(default)]
    pub arg_candidates: Vec<Vec<String>>,
}

/// Parse a probe-rules document. Strict, like the catalog: a malformed or
/// typo'd rules file errors instead of quietly probing with nothing.
pub fn parse_probe_rules(text: &str) -> Result<ProbeRules> {
    toml::from_str(text)
        .map_err(|e| NewtError::Config(format!("MCP probe rules are not valid TOML: {e}")))
}

/// The rules bundled into the binary. Guarded by a unit test, so the
/// `expect` cannot fire on a shipped build.
#[must_use]
pub fn builtin_probe_rules() -> ProbeRules {
    parse_probe_rules(BUNDLED_RULES).expect("bundled MCP probe rules must parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_rules_parse_with_the_conventional_spellings_in_order() {
        let rules = builtin_probe_rules();
        let candidates: Vec<Vec<&str>> = rules
            .arg_candidates
            .iter()
            .map(|c| c.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            candidates,
            vec![vec!["stdio"], vec![], vec!["mcp"], vec!["serve"]],
            "order is the trial order — it is part of the contract"
        );
    }

    #[test]
    fn parse_is_strict_about_malformed_and_typoed_documents() {
        assert!(parse_probe_rules("not toml [").is_err());
        // A typo'd key must not read as an empty rule set.
        assert!(parse_probe_rules("argcandidates = [[\"stdio\"]]\n").is_err());
        // An explicitly empty document IS a valid "no candidates" override.
        assert!(parse_probe_rules("").unwrap().arg_candidates.is_empty());
    }
}
