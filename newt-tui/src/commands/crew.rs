//! `/crew` — in-session crew settings. Moved verbatim from the `dispatch_slash`
//! match in `lib.rs`.

use newt_core::agentic::print_newt;

use crate::run_crew_edit;

/// Handle the `/crew` command family. Always returns `Ok(true)`.
pub(crate) fn dispatch(arg1: &str, arg2: &str, color: bool, verbose: bool) -> anyhow::Result<bool> {
    match arg1 {
        // `/crew edit [name]` runs the same interactive settings form as
        // `newt crew --edit`. read_turn() drops the rich surface to cooked
        // mode before slash dispatch, so the form's line input works
        // in-session for both surfaces (no raw-mode wrestling here).
        "edit" => {
            let name = (!arg2.is_empty()).then_some(arg2);
            if let Err(e) = run_crew_edit(name, color) {
                print_newt(&format!("crew edit failed: {e}"), color, verbose);
            }
        }
        // Running a crew in-session (`/crew "<task>"`) is the separate
        // workflow-TUI step; today the slash only edits settings.
        "" => print_newt(
            "usage: /crew edit [name] — edit a crew's settings \
             (planner/navigator/triage loadouts, control loop, test, budgets)",
            color,
            verbose,
        ),
        other => print_newt(
            &format!("unknown /crew subcommand '{other}' — try /crew edit [name]"),
            color,
            verbose,
        ),
    }
    Ok(true)
}
