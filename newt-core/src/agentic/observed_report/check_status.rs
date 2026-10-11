//! The shell envelope describes the outer command, never a pipeline's children.
use super::Check;

impl Check {
    pub(super) fn status_label(&self) -> String {
        let exit = self
            .exit
            .map_or_else(|| "unavailable".into(), |code| code.to_string());
        let simple = agent_bridle::inspect_shell(&self.command)
            .ok()
            .is_some_and(|shell| shell.commands.len() == 1 && shell.constructs.is_empty());
        let failure = self.lines.iter().any(|line| {
            let line = line.trim();
            line.starts_with("error:")
                || line.starts_with("test result: FAILED")
                || (line.starts_with("Summary ") && failed_summary(line))
        });
        if !simple {
            let result = if failure {
                "; failure observed in captured output"
            } else if self
                .lines
                .iter()
                .any(|line| line.trim().starts_with("test result: ok."))
            {
                "; Cargo test result observed: ok (excerpt only)"
            } else {
                ""
            };
            return format!("pipeline exit {exit} (inner command status not observed){result}");
        }
        if failure {
            format!("Failed; exit {exit} (failure observed in captured output)")
        } else {
            format!("{}; exit {exit}", self.outcome)
        }
    }
}
fn failed_summary(line: &str) -> bool {
    static FAILURE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b[1-9][\d,]* failed\b").expect("fixed nextest failure grammar")
    });
    FAILURE.is_match(line)
}

/// Replay must recognize exactly the presentation forms emitted above, as well
/// as legacy simple-command labels. Unknown prose is not a report envelope.
pub(super) fn recognizes_label(text: &str) -> bool {
    let valid_exit = |exit: &str| exit == "unavailable" || exit.parse::<i64>().is_ok();
    if let Some(rest) = text.strip_prefix("pipeline exit ") {
        return rest
            .split_once(" (inner command status not observed)")
            .is_some_and(|(exit, result)| {
                valid_exit(exit)
                    && matches!(
                        result,
                        "" | "; failure observed in captured output"
                            | "; Cargo test result observed: ok (excerpt only)"
                    )
            });
    }
    if let Some(exit) = text
        .strip_prefix("Failed; exit ")
        .and_then(|s| s.strip_suffix(" (failure observed in captured output)"))
    {
        return valid_exit(exit);
    }
    text.split_once("; exit ").is_some_and(|(outcome, exit)| {
        !outcome.is_empty() && outcome.chars().all(|c| c.is_ascii_alphabetic()) && valid_exit(exit)
    })
}
