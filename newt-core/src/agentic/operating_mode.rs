//! Session-local operating-mode selection for `/mode auto`.
//!
//! The core loop knows nothing about the TUI's concrete mode enum. It receives
//! this narrow collaborator only when the human has selected `auto`, and the
//! model may use it to request one of the bounded working styles for a future
//! turn. The collaborator cannot change the current turn's disposition or
//! caveats, and the schema deliberately excludes `full-auto`.

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
    // #2051: the sentence about what a selection does to the turn in flight
    // is disposition vocabulary, owned by `disposition_voice`. It used to say
    // "never changes the authority or disposition of the current turn" — the
    // mechanism by name, plus a reminder that the model has no move.
    let scope = super::DispositionVoices::default().next_turn_scope;
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "select_operating_mode",
            "description": format!(
                "In /mode auto, select the working style for the next action-shaped turn. \
                 {scope} Choose chat, dev, admin, plan, or diagnose; full-auto is human-only."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["chat", "dev", "admin", "plan", "diagnose"],
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
    }
}
