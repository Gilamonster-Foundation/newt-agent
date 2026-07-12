//! `/exit` · `/quit` · `/help` · `/version` · `/workspace` · `/config` — the
//! session-lifecycle and informational commands. Moved verbatim from the
//! `dispatch_slash` match in `lib.rs`.

use newt_core::agentic::print_newt;

use crate::{help_lines, VERSION};

/// Handle the lifecycle/info command family. `/exit`/`/quit` return `Ok(false)`
/// to end the session; everything else returns `Ok(true)`.
pub(crate) fn dispatch(
    cmd: &str,
    arg1: &str,
    workspace: &str,
    color: bool,
    verbose: bool,
) -> anyhow::Result<bool> {
    match cmd {
        "exit" | "quit" => return Ok(false),

        "help" => {
            print_newt("Available commands:", color, verbose);
            for line in help_lines() {
                println!("{line}");
            }
        }

        "version" => print_newt(&format!("v{VERSION}"), color, verbose),

        "workspace" => print_newt(workspace, color, verbose),

        // `/config show` dumps the resolved config; bare `/config` is a stub
        // (Claude-Code parity: `/config` there is a settings UI). A full
        // settings interface is future work — until then, bare `/config`
        // points at `/config show` rather than silently dumping.
        "config" => match arg1 {
            "show" => match newt_core::Config::resolve() {
                Ok(cfg) => match cfg.to_redacted_toml() {
                    Ok(toml_str) => {
                        print_newt("Resolved config (secrets redacted):", color, verbose);
                        println!("{toml_str}");
                    }
                    Err(e) => {
                        print_newt(&format!("error serializing config: {e}"), color, verbose);
                    }
                },
                Err(e) => print_newt(&format!("error resolving config: {e}"), color, verbose),
            },
            _ => print_newt(
                "settings interface not implemented yet — try `/config show` to dump the resolved config",
                color,
                verbose,
            ),
        },

        other => unreachable!("commands::meta::dispatch routed a non-meta command: {other:?}"),
    }
    Ok(true)
}
