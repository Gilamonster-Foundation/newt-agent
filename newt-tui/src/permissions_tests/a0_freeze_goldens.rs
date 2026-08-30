use super::*;
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

/// C0a (#1856): these frozen strings are now produced by
/// `newt_core::markup::plain::render`. The GOLDENS DID NOT MOVE — that
/// is the entire acceptance criterion of that slice, and this helper
/// changing while every string below stayed byte-identical is the proof.
fn text(req: &PermissionRequest, audience: Audience) -> String {
    let table = danger::DangerTable::builtin();
    plain::render(&permission_definition(req, &table, audience))
}

#[test]
fn terminal_low_net_offers_every_grant_including_the_permanents() {
    assert_eq!(
            text(&net_low(), Audience::Terminal),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [s]ession allow\n\
             [A]llow permanently (adds host to config)\n\
             [d]eny (default)\n\
             [D]eny always\n\
             [P]ermanently deny"
        );
}

#[test]
fn web_low_net_omits_every_durable_grant() {
    assert_eq!(
            text(&net_low(), Audience::Web),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [s]ession allow\n\
             [d]eny (default)"
        );
}

#[test]
fn terminal_high_exec_refuses_session_allow_and_says_why() {
    assert_eq!(
        text(&exec_high(), Audience::Terminal),
        "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
             \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
             (exec of \"bash\" is not within the granted authority)\n\
             high-danger: session allow refused; key allow / step-up is the future path, P3\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [d]eny (default)\n\
             [D]eny always\n\
             [P]ermanently deny"
    );
}

#[test]
fn web_high_exec_is_allow_once_or_deny_only() {
    assert_eq!(
        text(&exec_high(), Audience::Web),
        "\u{2298} run_command wants to run `bash` \u{2014} outside the granted exec allowlist.\n\
             \u{26a0} `bash` is an interpreter: this grants arbitrary command execution\n  \
             (exec of \"bash\" is not within the granted authority)\n\
             High danger: session authorization is unavailable.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [d]eny (default)"
    );
}

/// The control hint every prompt note carries; the goldens above embed it,
/// and the free-text form (`prompt_user_input`) reuses the same constant.
#[test]
fn the_modal_control_hint_is_the_frozen_control_vocabulary() {
    assert_eq!(MODAL_CONTROL_HINT, "Esc=back \u{b7} Ctrl-C/Ctrl-D=exit");
}
