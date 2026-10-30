//! #2959: the stateless part of the per-turn system message, shared by the
//! live turn and the `/context request` preview. Pure over borrowed text, so
//! the preview cannot touch session state by going through it.

use newt_core::agentic::request_report::Annotation;

pub(crate) struct TurnSystemParts<'a> {
    pub ws_state: &'a str,
    pub session_cwd: &'a std::path::Path,
    pub runtime: &'a crate::RuntimeContextParts,
    pub system: &'a str,
    pub session_controls: &'a str,
    /// Empty adds no bytes.
    pub communication_style: &'a str,
    pub is_clarification_answer: bool,
    /// `[prompt] components`: a piece whose component is off is not sent.
    pub components: &'a newt_core::prompt_components::ComponentSet,
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
    let on = |id: &str| p.components.is_on_id(id);
    let cwd_sentence = format!(
        "Current command working directory: {}. \
         run_command without cwd starts here. Explicit relative cwd and \
         relative file-tool paths resolve from the workspace root. \
         This does not change granted access.",
        serde_json::json!(p.session_cwd),
    );
    // Blocks are joined by a blank line. The runtime pieces abut inside one block.
    let mut notes = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    if on("workspace-state") {
        notes.push(note(
            "workspace-state",
            "newt-tui workspace_state_block",
            p.ws_state,
        ));
        blocks.push(p.ws_state.to_string());
    }
    notes.push(note(
        "working directory",
        "newt-tui assemble_turn_system",
        &cwd_sentence,
    ));
    blocks.push(cwd_sentence);
    let mut runtime = String::new();
    for (id, text) in p.runtime.pieces() {
        if on(id) && !text.is_empty() {
            notes.push(note(id, "newt-tui runtime_context_parts", text));
            runtime.push_str(text);
        }
    }
    if !runtime.is_empty() {
        blocks.push(runtime);
    }
    blocks.push(p.system.to_string());
    if on("operating-mode") {
        notes.push(note(
            "operating-mode",
            "newt-tui session_control_prompt",
            p.session_controls,
        ));
        blocks.push(p.session_controls.to_string());
    }
    let mut text = blocks.join("\n\n");
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
    use newt_core::prompt_components::ComponentSet;

    fn runtime() -> crate::RuntimeContextParts {
        crate::RuntimeContextParts {
            environment: "ENV".into(),
            runtime_authority: "AUTH".into(),
            git_identity: "GIT".into(),
            fs_confinement: "FS".into(),
        }
    }

    fn parts<'a>(
        style: &'a str,
        clar: bool,
        runtime: &'a crate::RuntimeContextParts,
        components: &'a ComponentSet,
    ) -> TurnSystemParts<'a> {
        TurnSystemParts {
            ws_state: "WS",
            session_cwd: std::path::Path::new("/w"),
            runtime,
            system: "SYS",
            session_controls: "CTL",
            communication_style: style,
            is_clarification_answer: clar,
            components,
        }
    }

    fn assembled(style: &str, clar: bool, set: &ComponentSet) -> TurnSystem {
        assemble_turn_system(&parts(style, clar, &runtime(), set))
    }

    #[test]
    fn pieces_join_in_turn_order_and_style_is_optional() {
        let all = ComponentSet::default();
        let t = assembled("", false, &all);
        assert!(t
            .text
            .starts_with("WS\n\nCurrent command working directory: \"/w\"."));
        assert!(t.text.ends_with("ENVAUTHGITFS\n\nSYS\n\nCTL"));
        // workspace-state, cwd, four runtime pieces, operating-mode.
        assert_eq!(t.notes.len(), 7);
        let t = assembled("STYLE", false, &all);
        assert!(t.text.ends_with("CTL\n\nSTYLE"));
        assert_eq!(t.notes.last().unwrap().label, "communication style");
    }

    /// #2933 PR5: each piece's component gates exactly that piece; the joins around
    /// the rest are unchanged.
    #[test]
    fn a_component_off_drops_exactly_its_piece() {
        let drop = |id: &str| {
            let set = ComponentSet::default().with(id, false).unwrap();
            assembled("", false, &set).text
        };
        let full = assembled("", false, &ComponentSet::default()).text;
        let cwd = "Current command working directory: \"/w\". run_command without cwd starts here. Explicit relative cwd and relative file-tool paths resolve from the workspace root. This does not change granted access.";
        assert_eq!(full, format!("WS\n\n{cwd}\n\nENVAUTHGITFS\n\nSYS\n\nCTL"));
        assert_eq!(
            drop("workspace-state"),
            format!("{cwd}\n\nENVAUTHGITFS\n\nSYS\n\nCTL")
        );
        assert_eq!(
            drop("environment"),
            format!("WS\n\n{cwd}\n\nAUTHGITFS\n\nSYS\n\nCTL")
        );
        assert_eq!(
            drop("runtime-authority"),
            format!("WS\n\n{cwd}\n\nENVGITFS\n\nSYS\n\nCTL")
        );
        assert_eq!(
            drop("git-identity"),
            format!("WS\n\n{cwd}\n\nENVAUTHFS\n\nSYS\n\nCTL")
        );
        assert_eq!(
            drop("fs-confinement"),
            format!("WS\n\n{cwd}\n\nENVAUTHGIT\n\nSYS\n\nCTL")
        );
        assert_eq!(
            drop("operating-mode"),
            format!("WS\n\n{cwd}\n\nENVAUTHGITFS\n\nSYS")
        );
        // All four runtime pieces off leave no empty block behind.
        let mut set = ComponentSet::default();
        for id in [
            "environment",
            "runtime-authority",
            "git-identity",
            "fs-confinement",
        ] {
            set = set.with(id, false).unwrap();
        }
        assert_eq!(
            assembled("", false, &set).text,
            format!("WS\n\n{cwd}\n\nSYS\n\nCTL")
        );
    }

    /// #2933: the cwd sentence is a named piece, so a turn's report has no
    /// unlabelled text; the joins between pieces are separators.
    #[test]
    fn every_byte_of_a_turn_is_labelled() {
        use newt_core::agentic::request_report::RequestReport;
        let some_off = ComponentSet::default()
            .with("environment", false)
            .unwrap()
            .with("operating-mode", false)
            .unwrap();
        for set in [ComponentSet::default(), some_off] {
            for (style, clar) in [("", false), ("STYLE", false), ("", true)] {
                let t = assembled(style, clar, &set);
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
    }

    #[test]
    fn clarification_wraps_the_head() {
        let t = assembled("", true, &ComponentSet::default());
        assert!(t.text.starts_with("<clarification_context>"));
        assert!(t.text.ends_with("ENVAUTHGITFS\n\nSYS\n\nCTL"));
    }
}
