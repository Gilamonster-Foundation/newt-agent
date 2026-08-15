//! `/prompt` · `/vi` · `/emacs` · `/nano` · `/edit-mode` · `/thinking` · `/nudge` ·
//! `/psyche` — the session-setting commands. The editor-family ones set an
//! `NEWT_*` env var a later step in `run_chat` picks up; `/psyche` owns every
//! effort dial (#1665): bare `/psyche` opens the panel (intercepted in
//! `chat.rs` on a rich TTY; here it renders the text status view for the
//! piped/lean path), and `/psyche cognition|tenacity <level>` are the text
//! setters that mutate the process-globals. The retired top-level
//! `/cognition` + `/tenacity` print a redirect and mutate NOTHING. Moved from
//! the `dispatch_slash` match in `lib.rs`.

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

        // #1665: retired top-levels. Redirect WITHOUT mutating — a habitual
        // `/tenacity relentless` must not half-work through a deprecation shim,
        // or the shim never gets to die.
        "tenacity" | "cognition" => print_newt(&retired_dial_redirect(cmd), color, verbose),

        // /psyche owns the dials (#1665): bare = panel (intercepted in chat.rs
        // on a rich TTY; HERE bare renders the text status for piped/lean),
        // `status` = text view, `cognition`/`tenacity` = text setters,
        // `obsessive` = max the live dials. Subcommand args live past arg1, so
        // re-derive the full remainder from the raw input.
        "psyche" => {
            let rest = input
                .trim_start()
                .trim_start_matches('/')
                .strip_prefix("psyche")
                .unwrap_or("")
                .trim();
            print_newt(&psyche_command(rest), color, verbose);
        }

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
    use newt_core::tenacity::{clear_cli_tenacity, effective_tenacity, set_cli_tenacity, Tenacity};
    match arg.trim() {
        "" | "status" | "show" => {
            let t = effective_tenacity();
            format!(
                "tenacity: {} — {}  (/psyche tenacity <auto|relaxed|standard|insistent|relentless>|list)",
                t.label(),
                t.describe()
            )
        }
        "list" => {
            let mut out = String::from("tenacity levels (patient → forcing):");
            out.push_str("\n  auto       inherit from the persona / config / model family");
            // Snapshot the active level ONCE. Re-reading `effective_tenacity()`
            // inside the loop let a concurrent override change (another thread /
            // test mutating the process-global) slip the "← active" marker off
            // every row — zero levels marked — because the compared value moved
            // between iterations. One read keeps the render internally consistent.
            let active_level = effective_tenacity();
            for t in Tenacity::all() {
                let active = if t == active_level { " ← active" } else { "" };
                out.push_str(&format!("\n  {:<10} {}{active}", t.label(), t.describe()));
            }
            out
        }
        // review-2 #2: clear the `--tenacity`/`/tenacity` override so tenacity
        // resolves from the persona / config / family again — the undo `/tenacity`
        // previously lacked (a session override could not be released).
        "auto" | "inherit" | "reset" => {
            clear_cli_tenacity();
            // #1668: releasing the dial is an operator action too — it UNPINS
            // the axis so the conversation follows the invocation baseline
            // again (an unpin the pin would otherwise have no way to express).
            newt_core::runtime::mark_tenacity_choice(None);
            format!(
                "tenacity → auto (override cleared) — now {} (from persona / config / family)",
                effective_tenacity().label()
            )
        }
        other => match other.parse::<Tenacity>() {
            Ok(level) => {
                set_cli_tenacity(level);
                // #1668: marked beside the setter, so only a PARSED level
                // pins — the error arm below mutates and marks nothing.
                newt_core::runtime::mark_tenacity_choice(Some(level));
                format!("tenacity → {} — {}", level.label(), level.describe())
            }
            Err(e) => {
                format!("{e}  (/psyche tenacity <auto|level>|list|status)")
            }
        },
    }
}

/// Build the `/cognition` response and, when `arg` names a level (or `off`/`auto`),
/// install the session override — the highest-priority input in
/// [`newt_core::cognition::resolve_cognition`], layered over the active persona's
/// `cognition:`. Pure for the show/list/error paths; the set paths mutate the
/// process-global via `set_cli_cognition`.
fn cognition_command(arg: &str) -> String {
    use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
    use newt_core::role_profile::Cognition;
    let usage = "(/psyche cognition <glancing|pondering|deliberating|contemplating>|off|auto|list)";
    match arg.trim() {
        "" | "status" | "show" => {
            match cli_cognition() {
                CognitionOverride::Unset => {
                    format!("cognition: auto — follows the active persona's `cognition:` (or off)  {usage}")
                }
                CognitionOverride::Off => {
                    format!("cognition: off — no reasoning controls sent, overriding any persona  {usage}")
                }
                CognitionOverride::Set(c) => format!(
                    "cognition: {} — {}  (session override, beats the persona)  {usage}",
                    c.label(),
                    c.describe()
                ),
            }
        }
        "list" => {
            let active = cli_cognition();
            let mut out = String::from("cognition levels (light → deep):");
            for c in Cognition::all() {
                let mark = if matches!(active, CognitionOverride::Set(a) if a == c) {
                    " ← override"
                } else {
                    ""
                };
                out.push_str(&format!("\n  {:<14} {}{mark}", c.label(), c.describe()));
            }
            out.push_str("\n  off            no reasoning controls (overrides the persona)");
            out.push_str("\n  auto           follow the active persona (default)");
            out
        }
        "off" | "none" => {
            set_cli_cognition(CognitionOverride::Off);
            // #1668: each of these three is an operator posture action on the
            // cognition axis — including `auto`, which UNPINS it.
            newt_core::runtime::mark_cognition_choice(CognitionOverride::Off);
            "cognition → off — no reasoning controls will be sent".to_string()
        }
        "auto" | "reset" | "persona" => {
            set_cli_cognition(CognitionOverride::Unset);
            newt_core::runtime::mark_cognition_choice(CognitionOverride::Unset);
            "cognition → auto — following the active persona".to_string()
        }
        other => match other.parse::<Cognition>() {
            Ok(level) => {
                set_cli_cognition(CognitionOverride::Set(level));
                newt_core::runtime::mark_cognition_choice(CognitionOverride::Set(level));
                format!("cognition → {} — {}", level.label(), level.describe())
            }
            Err(e) => format!("{e}  {usage}"),
        },
    }
}

/// The retired top-level dials' redirect line (#1665) — pure so the exact
/// wording is pinned by test: it must name the panel AND the text-setter form,
/// and say plainly that nothing changed.
fn retired_dial_redirect(cmd: &str) -> String {
    format!(
        "/{cmd} folded into /psyche — /psyche opens the dial panel; \
         text: /psyche {cmd} <level>  (nothing changed)"
    )
}

/// `/psyche` — every effort dial under one command (#1665). `rest` is the raw
/// remainder after "psyche": empty / `status` renders the text posture view
/// (bare `/psyche` only reaches here on the piped/lean path — a rich TTY
/// intercepts it in `chat.rs` and opens the panel), `cognition …` /
/// `tenacity …` delegate to the text setters, `obsessive` maxes the live
/// dials, and `edit` points at the panel's TTY requirement.
fn psyche_command(rest: &str) -> String {
    use newt_core::cognition::{cli_cognition, CognitionOverride};
    use newt_core::tenacity::effective_tenacity;
    let rest = rest.trim();
    let (sub, arg) = match rest.split_once(char::is_whitespace) {
        Some((s, a)) => (s, a.trim()),
        None => (rest, ""),
    };
    // `/psyche obsessive` — engage the max-everything posture's two LIVE dials.
    // Crew is a startup gate (crew_runner is built once at launch), so it can't
    // be turned on mid-session; say so honestly and point at the launch flag.
    if sub.eq_ignore_ascii_case("obsessive") || sub.eq_ignore_ascii_case("obsessive-relentless") {
        let (cog, ten) = newt_core::psyche::engage_obsessive_dials();
        // #1668: `obsessive` sets BOTH live dials, so it pins both axes.
        newt_core::runtime::mark_cognition_choice(newt_core::cognition::CognitionOverride::Set(
            cog,
        ));
        newt_core::runtime::mark_tenacity_choice(Some(ten));
        return format!(
            "obsessive engaged (live): cognition → {}, tenacity → {}.\n\
             crew is a launch gate — relaunch with `newt --obsessive` (or set \
             NEWT_TEAM) to add the crew this session.",
            cog.label(),
            ten.label()
        );
    }
    match sub {
        "cognition" => return cognition_command(arg),
        "tenacity" => return tenacity_command(arg),
        // Reached only where chat.rs did NOT open the panel (piped / lean).
        // Reached only when chat.rs did NOT open the panel: a lean build, a
        // piped session, or a malformed alias (extra arguments) — so the
        // message must never assert the terminal is incapable (review v2:
        // `/psyche edit extra` on a rich TTY landed here and was told its
        // interactive terminal wasn't one).
        "edit" => {
            return if arg.is_empty() {
                "/psyche opens the dial panel on the rich TUI with an \
                 interactive terminal; here, /psyche status shows the text \
                 view and /psyche cognition / /psyche tenacity <level> change \
                 the dials."
                    .to_string()
            } else {
                format!(
                    "usage: /psyche edit takes no arguments (got `{arg}`) — \
                     bare /psyche opens the dial panel"
                )
            }
        }
        "" | "status" | "show" => {}
        other => {
            return format!(
                "unknown /psyche subcommand `{other}` — usage: /psyche \
                 [status|cognition <level>|tenacity <level>|obsessive]"
            )
        }
    }
    // Show the EFFECTIVE cognition + where it resolves from (review-2 #6): a
    // status view, not just an override inspector.
    let cog = match cli_cognition() {
        CognitionOverride::Off => "off — no reasoning controls (session override)".to_string(),
        CognitionOverride::Set(c) => {
            format!("{} — {} (session override)", c.label(), c.describe())
        }
        CognitionOverride::Unset => match newt_core::cognition::persona_cognition() {
            Some(c) => format!("{} — {} (from the active persona)", c.label(), c.describe()),
            None => "auto — no reasoning controls (no persona sets it)".to_string(),
        },
    };
    let ten = effective_tenacity();
    // Mirror newt-cli's startup gate: the crew runner is built iff NEWT_TEAM is set.
    let crew = if std::env::var("NEWT_TEAM").is_ok() {
        "on"
    } else {
        "off"
    };
    let mut out = String::from("psyche — how hard the agent works (three orthogonal dials):");
    out.push_str(&format!("\n  cognition   {cog}"));
    out.push_str(
        "\n              backend-specific reasoning depth             (/psyche cognition)",
    );
    out.push_str(&format!(
        "\n  tenacity    {} — {}",
        ten.label(),
        ten.describe()
    ));
    out.push_str("\n              how hard the loop pushes read → act     (/psyche tenacity)");
    out.push_str(&format!("\n  crew        {crew}"));
    out.push_str(
        "\n              how many minds work the task                   (NEWT_TEAM / newt crew)",
    );
    out.push_str("\nobsessive = the max-everything posture: contemplating + relentless + crew on.");
    out.push_str(
        "\n/psyche — open the dial panel (rich TUI build + interactive terminal) · \
         /psyche cognition|tenacity <level> — text setters.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{cognition_command, tenacity_command};

    #[test]
    fn cognition_status_list_set_off_auto_and_error_are_informative() {
        use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
        use newt_core::role_profile::Cognition;
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // Start clean; the guard restores the process-global on drop.
        let restore = cli_cognition();
        set_cli_cognition(CognitionOverride::Unset);

        // Status (unset) explains it follows the persona and shows usage —
        // pointing at the /psyche subcommand form, not the retired top-level.
        let status = cognition_command("");
        assert!(status.starts_with("cognition: auto"), "{status}");
        assert!(status.contains("/psyche cognition"), "{status}");
        // List enumerates every level plus off/auto.
        let list = cognition_command("list");
        for label in [
            "glancing",
            "pondering",
            "deliberating",
            "contemplating",
            "off",
            "auto",
        ] {
            assert!(list.contains(label), "list missing {label}: {list}");
        }
        // Setting a level installs the override live.
        let msg = cognition_command("contemplating");
        assert!(msg.contains("contemplating"), "{msg}");
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Set(Cognition::Contemplating)
        );
        // `off` forces the off state; `auto` returns to following the persona.
        cognition_command("off");
        assert_eq!(cli_cognition(), CognitionOverride::Off);
        cognition_command("auto");
        assert_eq!(cli_cognition(), CognitionOverride::Unset);
        // An unknown level explains itself rather than silently doing nothing.
        let err = cognition_command("banana");
        assert!(err.contains("unknown cognition"), "{err}");

        set_cli_cognition(restore);
    }

    #[test]
    fn psyche_panel_shows_all_three_dials_and_how_to_change_them() {
        let out = super::psyche_command("");
        for k in ["cognition", "tenacity", "crew", "obsessive"] {
            assert!(out.contains(k), "psyche panel missing '{k}': {out}");
        }
        assert!(
            out.contains("/psyche cognition") && out.contains("/psyche tenacity"),
            "status view points at the /psyche subcommand setters: {out}"
        );
        assert!(
            !out.contains("/psyche edit"),
            "bare /psyche IS the panel now — the footer must not advertise \
             the old edit subcommand: {out}"
        );
    }

    #[test]
    fn psyche_obsessive_engages_the_max_live_dials_and_notes_crew() {
        use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
        use newt_core::role_profile::Cognition;
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        // Reset to a non-obsessive baseline so the assertions are meaningful.
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);

        let out = super::psyche_command("obsessive");
        // The two live dials are actually maxed.
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Set(Cognition::Contemplating)
        );
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
        // The message is honest about crew being a launch gate.
        assert!(out.to_lowercase().contains("crew"), "{out}");
        assert!(out.contains("--obsessive"), "{out}");

        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
    }

    #[test]
    fn tenacity_status_and_list_and_error_are_informative() {
        use newt_core::tenacity::{set_cli_tenacity, Tenacity};
        // Hold the global-settings lock and pin a known level: sibling tests
        // mutate the process-global tenacity override, and `list` marks whichever
        // level is *active*. Without the guard this read raced the mutators and
        // the "← active" marker intermittently vanished once the hosted runners
        // upped test parallelism (the CPU-capped self-hosted pods never hit it).
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Standard);
        // Status names the active level and the usage hint (no mutation).
        let status = tenacity_command("");
        assert!(status.starts_with("tenacity: "), "{status}");
        assert!(status.contains("/psyche tenacity"), "{status}");
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
    fn psyche_subcommands_route_to_the_dial_setters() {
        // #1665: /psyche cognition|tenacity <level> are the text setters — the
        // same functions the retired top-levels used, reached through /psyche.
        use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
        use newt_core::role_profile::Cognition;
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);

        let msg = super::psyche_command("cognition deliberating");
        assert!(msg.contains("deliberating"), "{msg}");
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Set(Cognition::Deliberating)
        );
        let msg = super::psyche_command("tenacity relentless");
        assert!(msg.contains("relentless"), "{msg}");
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
        // Bare subcommand = that dial's status view, no mutation.
        let status = super::psyche_command("cognition");
        assert!(status.starts_with("cognition:"), "{status}");
        // `status`/`show`/empty render the posture view; `edit` explains the
        // TTY requirement (only reached where chat.rs did not open the panel).
        for rest in ["", "status", "show"] {
            let out = super::psyche_command(rest);
            assert!(out.contains("psyche — how hard"), "{rest:?}: {out}");
        }
        let edit = super::psyche_command("edit");
        assert!(edit.contains("dial panel"), "{edit}");
        assert!(
            !edit.contains("needs an interactive rich terminal"),
            "review v2: the arm must not assert the terminal is incapable \
             (it also fires for malformed aliases ON a rich TTY): {edit}"
        );
        // Extra arguments are a usage error, not a lecture about terminals.
        let extra = super::psyche_command("edit extra");
        assert!(extra.contains("takes no arguments"), "{extra}");
        // An unknown subcommand explains itself.
        let err = super::psyche_command("banana");
        assert!(err.contains("unknown /psyche subcommand"), "{err}");

        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
    }

    /// #1668: the dial setters mark a posture ACTION on exactly the axis they
    /// set — and the read-only / refused forms mark nothing, which is what
    /// keeps merely LOOKING at the dials out of a conversation's pin. Driven
    /// through the real `/psyche` seam, not a hand-copy of the mark calls.
    #[test]
    fn psyche_setters_mark_exactly_the_axis_they_change() {
        use newt_core::cognition::CognitionOverride;
        use newt_core::role_profile::Cognition;
        use newt_core::runtime::drain_preference_actions;
        use newt_core::tenacity::Tenacity;
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let _ = drain_preference_actions();

        // Status / list / unknown / error forms: read-only, so no action.
        for rest in [
            "",
            "status",
            "cognition",
            "cognition list",
            "cognition transcending",
            "tenacity",
            "tenacity list",
            "tenacity nonsense",
            "banana",
            "edit",
        ] {
            let _ = super::psyche_command(rest);
            assert!(
                drain_preference_actions().is_empty(),
                "`/psyche {rest}` must not pin anything"
            );
        }

        // A level sets exactly its own axis.
        let _ = super::psyche_command("cognition deliberating");
        let a = drain_preference_actions();
        assert_eq!(
            a.cognition,
            Some(CognitionOverride::Set(Cognition::Deliberating))
        );
        assert_eq!((a.tenacity, a.backend, a.model), (None, None, None));

        let _ = super::psyche_command("tenacity relentless");
        let a = drain_preference_actions();
        assert_eq!(a.tenacity, Some(Some(Tenacity::Relentless)));
        assert_eq!(a.cognition, None);

        // `off` and `auto` are actions too — `auto` UNPINS the axis.
        let _ = super::psyche_command("cognition off");
        assert_eq!(
            drain_preference_actions().cognition,
            Some(CognitionOverride::Off)
        );
        let _ = super::psyche_command("cognition auto");
        assert_eq!(
            drain_preference_actions().cognition,
            Some(CognitionOverride::Unset)
        );
        let _ = super::psyche_command("tenacity auto");
        assert_eq!(drain_preference_actions().tenacity, Some(None));

        // `obsessive` sets BOTH live dials, so it pins both.
        let _ = super::psyche_command("obsessive");
        let a = drain_preference_actions();
        assert!(a.cognition.is_some() && a.tenacity.is_some(), "{a:?}");
        assert_eq!((a.backend, a.model), (None, None), "crew/backend untouched");
    }

    #[test]
    fn the_redirect_line_names_both_the_panel_and_the_text_setter() {
        // #1665 review: the redirect wording was previously unasserted anywhere.
        for cmd in ["tenacity", "cognition"] {
            let line = super::retired_dial_redirect(cmd);
            assert!(line.contains("folded into /psyche"), "{line}");
            assert!(line.contains(&format!("/psyche {cmd} <level>")), "{line}");
            assert!(line.contains("(nothing changed)"), "{line}");
        }
    }

    #[test]
    fn retired_top_level_dials_redirect_without_mutating() {
        // #1665: `/tenacity relentless` and `/cognition contemplating` must NOT
        // half-work through the deprecation shim — redirect only, zero writes.
        use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
        use newt_core::tenacity::{cli_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);

        super::dispatch(
            "tenacity",
            "relentless",
            "/tenacity relentless",
            ".",
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            cli_tenacity(),
            Some(Tenacity::Standard),
            "retired /tenacity must not mutate the override"
        );
        super::dispatch(
            "cognition",
            "contemplating",
            "/cognition contemplating",
            ".",
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Unset,
            "retired /cognition must not mutate the override"
        );

        set_cli_tenacity(Tenacity::Standard);
    }

    #[test]
    fn tenacity_command_sets_the_level_live() {
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let restore = effective_tenacity();
        let msg = tenacity_command("relentless");
        assert!(msg.contains("relentless"), "{msg}");
        assert_eq!(effective_tenacity(), Tenacity::Relentless);
        // review-2 #2: `/tenacity auto` releases the override (was un-undoable).
        let msg = tenacity_command("auto");
        assert!(msg.contains("cleared"), "{msg}");
        assert_eq!(
            newt_core::tenacity::cli_tenacity(),
            None,
            "/tenacity auto clears the session override"
        );
        // `inherit` / `reset` are aliases for the same clear.
        set_cli_tenacity(Tenacity::Insistent);
        tenacity_command("reset");
        assert_eq!(newt_core::tenacity::cli_tenacity(), None);
        // Restore so the process-global doesn't leak into sibling tests.
        set_cli_tenacity(restore);
    }
}
