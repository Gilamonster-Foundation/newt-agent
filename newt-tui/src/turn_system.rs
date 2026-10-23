//! #2959: the stateless part of the per-turn system message, shared by the
//! live turn and the `/context request` preview. Pure over borrowed text, so
//! the preview cannot touch session state by going through it.

use newt_core::agentic::request_report::Annotation;

pub(crate) struct TurnSystemParts<'a> {
    pub ws_state: &'a str,
    pub session_cwd: &'a std::path::Path,
    pub runtime_ctx: &'a str,
    pub system: &'a str,
    pub session_controls: &'a str,
    /// Empty adds no bytes.
    pub communication_style: &'a str,
    pub is_clarification_answer: bool,
}

pub(crate) struct TurnSystem {
    pub text: String,
    /// Label and source of each piece, in assembly order, for the request report.
    pub notes: Vec<Annotation>,
}

/// The session's semantic config with the pulled default embedding model
/// adopted (#1279). Shared by the live turn and the preview so both agree on
/// whether `code_search` is advertised.
pub(crate) fn session_semantic_config(cfg: &newt_core::Config) -> newt_core::SemanticConfig {
    let mut semantic = cfg
        .context
        .as_ref()
        .map(|c| c.semantic.clone())
        .unwrap_or_default();
    semantic.embedding_model_path = crate::effective_embedding_model_path(
        semantic.embedding_model_path.take(),
        newt_inference::palette::embed_model_dir_if_present(),
    );
    semantic
}

fn note(label: &str, source: &str, text: &str) -> Annotation {
    Annotation {
        label: label.into(),
        source: source.into(),
        text: text.into(),
    }
}

pub(crate) fn assemble_turn_system(p: &TurnSystemParts<'_>) -> TurnSystem {
    let mut notes = vec![
        note(
            "workspace state",
            "newt-tui workspace_state_block",
            p.ws_state,
        ),
        note(
            "runtime context",
            "newt-tui runtime_context_block",
            p.runtime_ctx,
        ),
        note(
            "session controls",
            "newt-tui session_control_prompt",
            p.session_controls,
        ),
    ];
    let cwd_sentence = format!(
        "Current command working directory: {}. \
         run_command without cwd starts here. Explicit relative cwd and \
         relative file-tool paths resolve from the workspace root. \
         This does not change granted access.",
        serde_json::json!(p.session_cwd),
    );
    notes.insert(
        1,
        note(
            "working directory",
            "newt-tui assemble_turn_system",
            &cwd_sentence,
        ),
    );
    let mut text = format!(
        "{}\n\n{cwd_sentence}\n\n{}\n\n{}\n\n{}",
        p.ws_state, p.runtime_ctx, p.system, p.session_controls,
    );
    if !p.communication_style.is_empty() {
        notes.push(note(
            "communication style",
            "newt-tui personality prompt_block",
            p.communication_style,
        ));
        text.push_str("\n\n");
        text.push_str(p.communication_style);
    }
    if p.is_clarification_answer {
        text = format!(
            "<clarification_context>\n\
             This accepted operator line is a clarification continuation. The protected prompt card names its objective root; use prompt_read {{\"address\":\"root\"}} to re-read the original objective before acting whenever the answer alone is insufficient.\n\
             </clarification_context>\n\n{text}"
        );
    }
    TurnSystem { text, notes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts<'a>(style: &'a str, clar: bool) -> TurnSystemParts<'a> {
        TurnSystemParts {
            ws_state: "WS",
            session_cwd: std::path::Path::new("/w"),
            runtime_ctx: "RT",
            system: "SYS",
            session_controls: "CTL",
            communication_style: style,
            is_clarification_answer: clar,
        }
    }

    #[test]
    fn pieces_join_in_turn_order_and_style_is_optional() {
        let t = assemble_turn_system(&parts("", false));
        assert!(t
            .text
            .starts_with("WS\n\nCurrent command working directory: \"/w\"."));
        assert!(t.text.ends_with("RT\n\nSYS\n\nCTL"));
        assert_eq!(t.notes.len(), 4);
        let t = assemble_turn_system(&parts("STYLE", false));
        assert!(t.text.ends_with("CTL\n\nSTYLE"));
        assert_eq!(t.notes.last().unwrap().label, "communication style");
    }

    /// #2933: the cwd sentence is a named piece, so a turn's report has no
    /// unlabelled text; the joins between pieces are separators.
    #[test]
    fn every_byte_of_a_turn_is_labelled() {
        use newt_core::agentic::request_report::RequestReport;
        for (style, clar) in [("", false), ("STYLE", false), ("", true)] {
            let t = assemble_turn_system(&parts(style, clar));
            // The frozen system text is labelled by its own builder; stand in.
            let mut notes = t.notes.clone();
            notes.push(newt_core::agentic::request_report::Annotation {
                label: "system".into(),
                source: "test".into(),
                text: "SYS".into(),
            });
            let report = RequestReport::build_from_parts(&[t.text.as_str()], None, &notes);
            let labels: Vec<&str> = report.system[0]
                .components
                .iter()
                .map(|c| c.label.as_str())
                .collect();
            // The clarification wrapper is the one piece without a label.
            let stray = labels.iter().filter(|l| **l == "(unattributed)").count();
            assert_eq!(stray, usize::from(clar), "{labels:?}");
        }
    }

    #[test]
    fn clarification_wraps_the_head() {
        let t = assemble_turn_system(&parts("", true));
        assert!(t.text.starts_with("<clarification_context>"));
        assert!(t.text.ends_with("RT\n\nSYS\n\nCTL"));
    }
}
