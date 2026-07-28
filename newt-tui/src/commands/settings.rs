//! `/prompt` · `/vi` · `/emacs` · `/nano` · `/edit-mode` · `/thinking` · `/nudge` — the
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

        "nudge" => match arg1 {
            "off" => {
                // SAFETY: single-threaded REPL; next turn's ChatCtx reads it.
                unsafe { std::env::set_var("NEWT_NUDGE", "off") };
                print_newt(
                    "action-pressure nudges OFF for this session (narration rescue, \
                     workflow repair steering, plan pushes) — factual corrections stay on",
                    color,
                    verbose,
                );
            }
            "on" => {
                // SAFETY: single-threaded REPL.
                unsafe { std::env::remove_var("NEWT_NUDGE") };
                print_newt("action-pressure nudges ON (default)", color, verbose);
            }
            "" | "status" => {
                let off = std::env::var("NEWT_NUDGE").is_ok_and(|v| v.eq_ignore_ascii_case("off"));
                print_newt(
                    &format!(
                        "action-pressure nudges: {}  (/nudge <on|off>)",
                        if off { "OFF" } else { "on" }
                    ),
                    color,
                    verbose,
                );
            }
            _ => print_newt("usage: /nudge <on|off|status>", color, verbose),
        },

        "thinking" => match arg1 {
            "on" | "off" => {
                // SAFETY: single-threaded REPL.
                unsafe { std::env::set_var("NEWT_THINKING", arg1) };
                print_newt(&format!("thinking spinner: {arg1}"), color, verbose);
            }
            _ => print_newt("usage: /thinking <on|off>", color, verbose),
        },

        // #tenacity / #12: how hard the harness pushes the model from reading to
        // acting. `/tenacity` shows the active level; `/tenacity <level>` sets an
        // explicit override that wins over `[tenacity]` config + the CLI flag.
        "tenacity" => print_newt(&tenacity_command(arg1), color, verbose),

        other => {
            unreachable!("commands::settings::dispatch routed a non-setting command: {other:?}")
        }
    }
    Ok(true)
}

/// Build the `/tenacity` response and, when `arg` names a level, install it as an
/// explicit override (the highest-priority input in
/// [`newt_core::tenacity::effective_tenacity`]). Pure for the show/list/error
/// paths; the set path mutates the process-global via `set_cli_tenacity`.
fn tenacity_command(arg: &str) -> String {
    use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
    match arg.trim() {
        "" | "status" | "show" => {
            let t = effective_tenacity();
            format!(
                "tenacity: {} — {}  (/tenacity <relaxed|standard|insistent|relentless>|list)",
                t.label(),
                t.describe()
            )
        }
        "list" => {
            let mut out = String::from("tenacity levels (patient → forcing):");
            for t in Tenacity::all() {
                let active = if t == effective_tenacity() {
                    " ← active"
                } else {
                    ""
                };
                out.push_str(&format!("\n  {:<10} {}{active}", t.label(), t.describe()));
            }
            out
        }
        other => match other.parse::<Tenacity>() {
            Ok(level) => {
                set_cli_tenacity(level);
                format!("tenacity → {} — {}", level.label(), level.describe())
            }
            Err(e) => {
                format!("{e}  (/tenacity <level>|list|status)")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::tenacity_command;

    #[test]
    fn tenacity_status_and_list_and_error_are_informative() {
        // Status names the active level and the usage hint (no mutation).
        let status = tenacity_command("");
        assert!(status.starts_with("tenacity: "), "{status}");
        assert!(status.contains("/tenacity"), "{status}");
        // List enumerates every level, patient → forcing, marking the active one.
        let list = tenacity_command("list");
        for label in ["relaxed", "standard", "insistent", "relentless"] {
            assert!(list.contains(label), "list missing {label}: {list}");
        }
        assert!(
            list.contains("← active"),
            "list marks the active level: {list}"
        );
        // An unknown level explains itself rather than silently doing nothing.
        let err = tenacity_command("banana");
        assert!(err.contains("unknown tenacity"), "{err}");
    }

    #[test]
    fn tenacity_command_sets_the_level_live() {
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let restore = effective_tenacity();
        let msg = tenacity_command("relentless");
        assert!(msg.contains("relentless"), "{msg}");
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
        // Restore so the process-global doesn't leak into sibling tests.
        set_cli_tenacity(restore);
    }
}
