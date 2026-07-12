//! `/exit` · `/quit` · `/help` · `/version` · `/workspace` · `/config` — the
//! session-lifecycle and informational commands. Moved verbatim from the
//! `dispatch_slash` match in `lib.rs`.

use newt_core::agentic::print_newt;

use crate::{help_lines, VERSION};

/// Handle the lifecycle/info command family. `/exit`/`/quit` return `Ok(false)`
/// to end the session; everything else returns `Ok(true)`.
pub(crate) fn dispatch(
    cmd: &str,
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

        "config" => match newt_core::Config::resolve() {
            Ok(cfg) => match cfg.to_redacted_toml() {
                Ok(toml_str) => {
                    print_newt("Resolved config (secrets redacted):", color, verbose);
                    println!("{toml_str}");
                }
                Err(e) => print_newt(&format!("error serializing config: {e}"), color, verbose),
            },
            Err(e) => print_newt(&format!("error resolving config: {e}"), color, verbose),
        },

        other => unreachable!("commands::meta::dispatch routed a non-meta command: {other:?}"),
    }
    Ok(true)
}
