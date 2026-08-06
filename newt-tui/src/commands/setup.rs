//! `/setup` — configure an inference backend from inside a session.
//!
//! Delegates to the CLI `newt setup` via [`crate::run_newt_subcmd`] (the
//! crew.rs pattern): the subprocess gets its own tokio runtime and terminal
//! lifecycle, and `newt setup` stays the single canonical implementation.
//! read_turn() already dropped the rich surface to cooked mode before slash
//! dispatch, so the wizard's line input works in-session on both surfaces.

use newt_core::agentic::print_newt;

use crate::run_newt_subcmd;

/// Handle `/setup [host-or-url]`. Always returns `Ok(true)`.
pub(crate) fn dispatch(arg1: &str, color: bool, verbose: bool) -> anyhow::Result<bool> {
    let result = if arg1.is_empty() {
        run_newt_subcmd(&["setup"], color, verbose)
    } else {
        run_newt_subcmd(&["setup", arg1], color, verbose)
    };
    if let Err(e) = result {
        print_newt(&format!("setup failed: {e}"), color, verbose);
    } else {
        print_newt(
            "backend changes apply to new sessions — /backends lists what's configured",
            color,
            verbose,
        );
    }
    Ok(true)
}
