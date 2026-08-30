use super::*;
use newt_core::markup::plain;
use newt_core::{DenialKind, PermissionRequest};

fn net_low() -> PermissionRequest {
    PermissionRequest {
        tool: "http".into(),
        kind: DenialKind::Net,
        target: "https://example.com/api".into(),
        reason: String::new(),
    }
}

fn exec_high() -> PermissionRequest {
    PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "bash".into(),
        reason: "exec of \"bash\" is not within the granted authority".into(),
    }
}

/// The four frozen forms, as `(request, audience, exact bytes)`.
fn goldens() -> Vec<(PermissionRequest, Audience, &'static str)> {
    vec![
            (
                net_low(),
                Audience::Terminal,
                "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
                 Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
                 [a]llow once\n\
             [s]ession allow\n\
             [A]llow permanently (adds host to config)\n\
             [d]eny (default)\n\
             [D]eny always\n\
             [P]ermanently deny",
            ),
            (
                net_low(),
                Audience::Web,
                "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
                 Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
                 [a]llow once\n\
             [s]ession allow\n\
             [d]eny (default)",
            ),
            (
                exec_high(),
                Audience::Terminal,
                "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
                 \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
                 (exec of \"bash\" is not within the granted authority)\n\
                 high-danger: session allow refused; key allow / step-up is the future path, P3\n\
                 Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
                 [a]llow once\n\
             [d]eny (default)\n\
             [D]eny always\n\
             [P]ermanently deny",
            ),
            (
                exec_high(),
                Audience::Web,
                "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
                 \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
                 (exec of \"bash\" is not within the granted authority)\n\
                 High danger: session authorization is unavailable.\n\
                 Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
                 [a]llow once\n\
             [d]eny (default)",
            ),
        ]
}

/// **The acceptance criterion.** `render()` is correct exactly when it
/// reproduces today's bytes — for every `(request, audience)` the
/// production builder can hand it.
#[test]
fn render_reproduces_every_a0_golden_byte_for_byte() {
    let table = danger::DangerTable::builtin();
    for (req, audience, golden) in goldens() {
        let definition = permission_definition(&req, &table, audience.clone());
        assert_eq!(
            plain::render(&definition),
            golden,
            "the plain renderer moved frozen bytes for {:?}/{audience:?}",
            req.tool
        );

        // ...and the string the operator actually sees. The glyph gets
        // its OWN final line: `tty::modal::render` repaints only the
        // text after the last `\n`, so a renderer that grew a trailing
        // newline would repaint the wrong row on every keystroke.
        let composed = format!("{}\n{MODAL_INPUT_GLYPH}", plain::render(&definition));
        assert_eq!(composed, format!("{golden}\n{MODAL_INPUT_GLYPH}"));
        assert!(
            composed.ends_with(&format!("\n{MODAL_INPUT_GLYPH}")),
            "the input glyph is not alone on the final line: {composed:?}"
        );
    }
}

/// **The free-text form's bytes, pinned for the first time** (A0 gap).
///
/// `prompt_user_input` built an actionless `Question` and rendered it
/// with `terminal_text`; deleting that method left it nothing to render
/// through, so it became an `InteractionDefinition` with a `Text`
/// control. A0's sweep recorded that NOTHING covered these bytes — only
/// the `MODAL_CONTROL_HINT` constant was pinned — so the byte-identity
/// claim for this path had no test to rest on. It does now.
///
/// The expectation is written out rather than derived: a `Text` control
/// contributes no choices line, so the form is body + note, exactly as
/// the actionless `Question` rendered.
#[test]
fn the_free_text_form_renders_exactly_as_it_did() {
    let form = InteractionDefinition {
        note: Some(MODAL_CONTROL_HINT.into()),
        ..InteractionDefinition::new(
            InteractionKind::Prompt,
            format!("? {}", "which file should I edit"),
            vec![Control {
                id: ControlId::new(ANSWER_CONTROL).expect("valid control id"),
                kind: ControlKind::Text,
                label: String::new(),
                requirement: Requirement::Required,
            }],
        )
    };
    assert_eq!(
        plain::render(&form),
        "? which file should I edit\nEsc=back \u{b7} Ctrl-C/Ctrl-D=exit"
    );
    // A text control offers nothing to pick, so no choices line appears.
    assert!(!plain::render(&form).contains('['));
}

/// **The web surface is untouched by C0a.** The terminal changed how it
/// RENDERS; the web still publishes the same definition, offers the same
/// actions in the same order, and reconstructs the same legacy form for
/// its HTML card. A "the terminal got faster" change that quietly
/// widened the web's action set is the regression this catches.
#[test]
fn the_web_matrix_is_unaffected() {
    let table = danger::DangerTable::builtin();

    let low = permission_definition(&net_low(), &table, Audience::Web);
    let ControlKind::Choice { options } = &low.controls[0].kind else {
        panic!("the decision control is not a choice");
    };
    assert_eq!(
        options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["allow_once", "allow_session", "deny"],
        "the web action matrix changed"
    );

    let high = permission_definition(&exec_high(), &table, Audience::Web);
    let ControlKind::Choice { options } = &high.controls[0].kind else {
        panic!("the decision control is not a choice");
    };
    assert_eq!(
        options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["allow_once", "deny"],
        "the web action matrix changed for a high-danger target"
    );

    // D0 (#1878): the web card is no longer built by reconstructing a
    // legacy form — C3c made it read the definition directly, and the
    // reverse adapter is deleted. What this test is FOR survives: that
    // the web matrix is what policy says, independent of the terminal's.
    for definition in [&low, &high] {
        let ControlKind::Choice { options } = &definition.controls[0].kind else {
            panic!("not a choice");
        };
        assert!(!options.is_empty(), "a web offer must offer something");
        for option in options {
            assert!(
                action_for_option(option.id.as_str()).is_some(),
                "every offered option names an action this build knows: {}",
                option.id.as_str()
            );
        }
    }
}
