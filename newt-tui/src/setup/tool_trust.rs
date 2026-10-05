//! First-run tool diagnostics; permissions require a separate explicit decision.
use super::{decide, interaction_form, Operator};
use newt_core::git_staging::tool_diagnostics::{diagnose_tool_paths, ToolTrustReport};

pub(super) fn check(op: &Operator<'_>) -> anyhow::Result<()> {
    let report = diagnose_tool_paths();
    use std::io::IsTerminal;
    let consent = if std::io::stdin().is_terminal() {
        review(op, &report)?
    } else {
        op.say("Push tool paths:");
        for line in &report.lines {
            op.say(line);
        }
        false
    };
    if !report.hints.is_empty() {
        for line in report.repair(consent) {
            op.say(&line);
        }
    }
    Ok(())
}

fn review(op: &Operator<'_>, report: &ToolTrustReport) -> anyhow::Result<bool> {
    op.say("Push tool paths:");
    for line in &report.lines {
        op.say(line);
    }
    if report.hints.is_empty() {
        return Ok(false);
    }
    decide(op, &interaction_form::confirm(
        "Remove the displayed group/other write permissions?",
        "Only these displayed chmod fixes will be applied. Homebrew may restore group write; newt doctor re-checks.",
        "yes, apply these fixes", "no, leave permissions unchanged",
    ), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::operator::Script;
    use newt_core::git_staging::TrustHint;

    /// #2739: setup's normal defaults never consent to permission changes.
    #[test]
    fn only_explicit_yes_authorizes_tool_repairs() {
        let hint = TrustHint {
            path: "/brew/bin".into(),
            mode: 0o775,
            chmod_arg: "g-w",
        };
        let report = ToolTrustReport {
            lines: vec![hint.render()],
            hints: vec![hint],
        };
        for (answers, expected) in [
            (&["yes"][..], true),
            (&["no"][..], false),
            (&[""][..], false),
            (&["maybe"][..], false),
        ] {
            let script = Script::new(answers);
            assert_eq!(
                review(&script.operator(), &report).unwrap_or(false),
                expected
            );
            assert!(script
                .output
                .borrow()
                .iter()
                .any(|s| s.contains("chmod g-w /brew/bin")));
        }
    }
}
