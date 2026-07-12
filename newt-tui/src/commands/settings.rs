//! `/prompt` · `/vi` · `/emacs` · `/nano` · `/edit-mode` · `/thinking` — the
//! session-setting commands, each of which sets an `NEWT_*` env var that a
//! later step in `run_chat` picks up. Moved verbatim from the `dispatch_slash`
//! match in `lib.rs`.

use newt_core::agentic::print_newt;

use crate::{current_prompt_and_preview, prompt_token_help, strip_one_quote_pair};

/// Handle the session-setting command family. Always returns `Ok(true)` (none
/// of these end the session).
pub(crate) fn dispatch(
    cmd: &str,
    arg1: &str,
    input: &str,
    workspace: &str,
    color: bool,
    verbose: bool,
) -> anyhow::Result<bool> {
    match cmd {
        "prompt" if arg1 == "set" => {
            // Everything after "prompt set " is the literal template — taken
            // from the RAW input so internal/trailing spaces survive, with one
            // layer of surrounding quotes stripped. Applies for the session
            // (via NEWT_PROMPT, which the per-turn prompt build reads first);
            // put it in `[tui] prompt` to persist.
            let template = input
                .trim_start_matches('/')
                .strip_prefix("prompt")
                .and_then(|s| s.trim_start().strip_prefix("set"))
                .map(|s| s.strip_prefix(' ').unwrap_or(s))
                .map(strip_one_quote_pair)
                .unwrap_or("");
            if template.is_empty() {
                print_newt(
                    "usage: /prompt set \"<template>\"  (try /prompt for the token list)",
                    color,
                    verbose,
                );
            } else {
                // SAFETY: single-threaded REPL; the next prompt is built right
                // after this returns.
                unsafe { std::env::set_var("NEWT_PROMPT", template) };
                let (_t, preview) = current_prompt_and_preview(workspace);
                print_newt(
                    &format!("prompt set for this session — preview: {preview}"),
                    color,
                    verbose,
                );
                print_newt(
                    "(add to [tui] prompt to persist — use $NAME macros there to avoid TOML escaping)",
                    color,
                    verbose,
                );
            }
        }

        "prompt" if matches!(arg1, "reset" | "default" | "clear") => {
            // SAFETY: single-threaded REPL.
            unsafe { std::env::remove_var("NEWT_PROMPT") };
            print_newt(
                "prompt reset to your [tui] prompt / the built-in default.",
                color,
                verbose,
            );
        }

        "prompt" => {
            print_newt(
                "Prompt tokens — `/prompt set \"<template>\"` to change, or `[tui] prompt` to persist:",
                color,
                verbose,
            );
            for line in prompt_token_help() {
                println!("{line}");
            }
            print_newt(
                "In config.toml prefer the $NAME macros — the \\x forms are eaten by TOML \
                 (use a 'literal string' or doubled \\\\).",
                color,
                verbose,
            );
            let (tmpl, preview) = current_prompt_and_preview(workspace);
            print_newt(&format!("current: {tmpl:?}"), color, verbose);
            print_newt(&format!("preview: {preview}"), color, verbose);
        }

        "vi" | "emacs" | "nano" | "edit-mode" => {
            // Switch the line-editor key bindings for the rest of the session.
            // Sets NEWT_EDIT_MODE; the editor rebuild + the is_vi/caret recompute
            // back in `run_chat` (after every slash command) pick it up.
            let want = match cmd {
                "vi" => Some("vi"),
                "emacs" => Some("emacs"),
                "nano" => Some("nano"),
                _ => match arg1.to_lowercase().as_str() {
                    "vi" | "vim" => Some("vi"),
                    "emacs" => Some("emacs"),
                    "nano" => Some("nano"),
                    _ => None,
                },
            };
            match want {
                Some(m) => {
                    // SAFETY: single-threaded REPL; the editor is rebuilt right
                    // after this returns, before any further input is read.
                    unsafe { std::env::set_var("NEWT_EDIT_MODE", m) };
                    print_newt(&format!("edit mode: {m}"), color, verbose);
                }
                None => print_newt(
                    "usage: /edit-mode <vi|emacs|nano>  (or just /vi, /emacs, /nano)",
                    color,
                    verbose,
                ),
            }
        }

        "thinking" => match arg1 {
            "on" | "off" => {
                // SAFETY: single-threaded REPL.
                unsafe { std::env::set_var("NEWT_THINKING", arg1) };
                print_newt(&format!("thinking spinner: {arg1}"), color, verbose);
            }
            _ => print_newt("usage: /thinking <on|off>", color, verbose),
        },

        other => {
            unreachable!("commands::settings::dispatch routed a non-setting command: {other:?}")
        }
    }
    Ok(true)
}
