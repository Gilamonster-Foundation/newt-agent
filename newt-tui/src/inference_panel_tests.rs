use super::*;
use crate::context_window::{ContextWindow, WindowSource};

fn window() -> ContextWindow {
    ContextWindow {
        full_window: Some(131_072),
        window_source: Some(WindowSource::Served),
        recovered_hard_window: None,
        safe_context: Some(104_857),
        max_ok_input: Some(100_703),
        num_ctx: Some(131_072),
    }
}

fn thinking_capability() -> ChatCompletionsCapability {
    ChatCompletionsCapability {
        cognition: Some(true),
        chat_template_kwargs: Some(true),
        ..ChatCompletionsCapability::default()
    }
}

fn inference<'a>(launch: Option<&'a LlamaCppLaunch>) -> Inference<'a> {
    Inference {
        model: "m-35b",
        backend: "lab-router",
        window: window(),
        input_ceiling_pct: 80,
        input_ceiling_pct_is_default: true,
        last_turn: Some((24_341, Some(104_857))),
        cognition: Some(Cognition::Thoughtful),
        cognition_source: "session override",
        capability: thinking_capability(),
        card: Some("m-35b"),
        card_inactive: None,
        preview: ChatGenerationPreview {
            enable_thinking: Some(true),
            temperature: Some(0.6),
            top_p: Some(0.95),
            max_output_tokens: None,
            output_allowance: None,
        },
        launch,
        launch_note: "not a llama.cpp router",
    }
}

fn the_row(lines: &[String], label: &str) -> String {
    lines
        .iter()
        .find(|l| l.starts_with(label))
        .unwrap_or_else(|| panic!("no `{label}` row in {lines:#?}"))
        .clone()
}

/// The motivating confusion: `/105k` on the status bar was the input ceiling,
/// not the window. The section shows both, and what each comes from.
#[test]
fn the_window_and_the_ceiling_are_separate_rows_with_their_sources() {
    let lines = lines(&inference(None));
    let window = the_row(&lines, "context window");
    assert!(
        window.contains("131,072") && window.contains("reported by the server"),
        "{window}"
    );
    let ceiling = the_row(&lines, "input ceiling");
    assert!(
        ceiling.contains("104,857") && ceiling.contains("80% of the window"),
        "{ceiling}"
    );
    assert!(ceiling.contains("(default, #2565)"), "{ceiling}");
    assert!(the_row(&lines, "last turn").contains("24,341 / 104,857"));
}

#[test]
fn thinking_on_says_what_is_sent_and_where_the_level_came_from() {
    let row = the_row(&lines(&inference(None)), "model thinking");
    assert!(row.contains("ON"), "{row}");
    assert!(
        row.contains("enable_thinking=true") && row.contains("thoughtful (session override)"),
        "{row}"
    );
}

/// #2566: every silent-off link is named, first missing link first.
#[test]
fn thinking_off_names_the_first_missing_link() {
    let off = |edit: &dyn Fn(&mut Inference<'_>)| {
        let mut i = inference(None);
        i.preview.enable_thinking = None;
        edit(&mut i);
        the_row(&lines(&i), "model thinking")
    };
    assert!(off(&|i| i.card = None).contains("no model card"));
    assert!(
        off(&|i| i.capability.chat_template_kwargs = None).contains("chat_template_kwargs = true")
    );
    assert!(off(&|i| i.capability.cognition = None).contains("cognition = true"));
    assert!(off(&|i| i.cognition = None).contains("/settings cognition"));
    let inactive =
        off(&|i| i.card_inactive = Some("card `m-35b` is bound to (no declared model)".into()));
    assert!(inactive.contains("no declared model"), "{inactive}");
}

#[test]
fn the_server_launch_declaration_is_listed_flag_by_flag() {
    let launch = LlamaCppLaunch {
        args: [
            "--chat-template-kwargs",
            "{\"enable_thinking\": false}",
            "--jinja",
            "--ctx-size",
            "131072",
        ]
        .map(String::from)
        .to_vec(),
        preset: None,
    };
    let lines = lines(&inference(Some(&launch)));
    let default = the_row(&lines, "  server default");
    assert!(default.contains("enable_thinking=false"), "{default}");
    assert!(
        lines.contains(&"  --jinja".to_string()),
        "a bare flag stands alone: {lines:#?}"
    );
    assert!(lines.contains(&"  --ctx-size 131072".to_string()));
    let none = lines_without_launch();
    assert!(the_row(&none, "server launch").contains("not a llama.cpp router"));
}

fn lines_without_launch() -> Vec<String> {
    lines(&inference(None))
}
