//! Session-local operating-mode selection for `/mode auto`.
//!
//! The core loop receives this narrow collaborator only when the human has
//! selected `auto`, and the model may use it to request one of the bounded
//! working styles for a future turn. The collaborator cannot change the
//! current turn's disposition or caveats, and the schema deliberately excludes
//! `full-auto`.
//!
//! The line above used to read "the core loop knows nothing about the TUI's
//! concrete mode enum", and the schema below carried its own hand-written copy
//! of the mode list as a result. The enum moved down in #2009 PR4b, so the
//! schema is now generated from `OperatingMode::model_selectable()` — one
//! vocabulary, and a mode added to the enum cannot be silently missing here.

/// Model-facing seam behind `select_operating_mode`.
///
/// Implementations own the session state and validate the requested name.
/// Selection applies to a later turn; callers must never reinterpret the
/// result as authority for the tool round already in flight.
pub trait OperatingModeControl: Send + Sync {
    /// Select a bounded working style for a future turn.
    fn select_operating_mode(&self, mode: &str) -> Result<String, String>;
}

/// Tool definition advertised only when an [`OperatingModeControl`] is
/// injected (the interactive session is configured with `/mode auto`).
pub fn select_operating_mode_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "select_operating_mode",
            "description": "In /mode auto, select the working style for the next \
                            action-shaped turn. This changes no permissions and \
                            never changes the authority or disposition of the \
                            current turn. Choose chat, dev, admin, plan, or \
                            diagnose; full-auto is human-only.",
            "parameters": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": crate::operating_mode::OperatingMode::model_selectable(),
                        "description": "The bounded working style to use next."
                    }
                },
                "required": ["mode"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::select_operating_mode_tool_definition;

    #[test]
    fn schema_excludes_human_only_modes() {
        let definition = select_operating_mode_tool_definition();
        let modes = definition["function"]["parameters"]["properties"]["mode"]["enum"]
            .as_array()
            .unwrap();
        assert!(modes.iter().any(|mode| mode == "dev"));
        assert!(!modes.iter().any(|mode| mode == "auto"));
        assert!(!modes.iter().any(|mode| mode == "full-auto"));

        // ...and it is the enum's own list, not a copy that happens to agree
        // today. A style added to `OperatingMode` now appears here or fails.
        let want = crate::operating_mode::OperatingMode::model_selectable();
        let got: Vec<&str> = modes.iter().filter_map(|m| m.as_str()).collect();
        assert_eq!(got, want, "the schema drifted from the mode vocabulary");
        assert!(
            !want.is_empty(),
            "an empty list would make every assertion above vacuous"
        );
    }
}
