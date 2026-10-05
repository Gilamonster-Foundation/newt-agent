//! #2719: source-bound output wrappers around a governed Git/gh command. No shell runs.
//! Inspection deliberately flattens topology, so we also consume every byte of
//! the source, accepting only `command [2>&1] [| head ...] [; echo ...]`.
use super::{inspect_shell, literal};

pub(super) const RETRY: &str = "Retry with run_command using the worktree as cwd and command `git push origin` (or `git push --dry-run origin` for a dry run).";

pub(super) struct GovernedCommand {
    pub argv: Vec<String>,
    head: Option<usize>,
    echo: Vec<String>,
}

impl GovernedCommand {
    pub fn parse(source: &str) -> Result<Self, &'static str> {
        let inspection = inspect_shell(source).map_err(|_| "unsupported command shell syntax")?;
        if !inspection.constructs.is_empty() {
            return Err("command wrappers cannot contain substitutions or arithmetic");
        }
        let mut remaining = source.trim();
        let mut result = Self {
            argv: vec![],
            head: None,
            echo: vec![],
        };
        for (index, command) in inspection.commands.iter().enumerate() {
            if !command.descendant_execs.is_empty() {
                return Err("command wrappers cannot execute descendants");
            }
            if index > 0 {
                let separator = if command.program.as_deref() == Some("head")
                    && result.head.is_none()
                    && result.echo.is_empty()
                {
                    "|"
                } else if command.program.as_deref() == Some("echo") && result.echo.is_empty() {
                    ";"
                } else {
                    return Err("only a head pipeline and a trailing echo may wrap a command");
                };
                remaining = remaining
                    .trim_start()
                    .strip_prefix(separator)
                    .ok_or("conditional or background command execution is unsupported")?
                    .trim_start();
            }
            remaining = remaining
                .strip_prefix(&command.source)
                .ok_or("command must be a foreground command without shell groups")?;
            // Reconcile the source with ALL argv words: assignments, prefix
            // redirects and other syntax absent from argv must not be discarded.
            let mut words = command.source.as_str();
            for word in &command.argv {
                words = words
                    .trim_start()
                    .strip_prefix(word)
                    .ok_or("environment assignments and prefix redirects are unsupported")?;
            }
            let redirect = words.trim();
            if !(redirect.is_empty() || index == 0 && redirect == "2>&1") {
                return Err("command wrappers may only redirect stderr to stdout (2>&1)");
            }
            if index == 0 {
                result.argv = command
                    .argv
                    .iter()
                    .map(|word| literal(word).ok_or("command arguments must be literal"))
                    .collect::<Result<_, _>>()?;
            } else if command.program.as_deref() == Some("head") {
                let args = command
                    .argv
                    .iter()
                    .skip(1)
                    .map(|word| literal(word).ok_or("head arguments must be literal"))
                    .collect::<Result<Vec<_>, _>>()?;
                let count = match args.as_slice() {
                    [] => 10,
                    [n] => n
                        .strip_prefix("-n")
                        .or_else(|| n.strip_prefix('-'))
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(0),
                    [flag, n] if flag == "-n" => n.parse().unwrap_or(0),
                    _ => 0,
                };
                if !(1..=10_000).contains(&count) {
                    return Err("head must select 1..10000 output lines without file operands");
                }
                result.head = Some(count);
            } else {
                if command.argv.len() < 2 {
                    return Err("trailing echo must contain literal text or the exit status");
                }
                for word in command.argv.iter().skip(1) {
                    let value =
                        echo_word(word, 0).ok_or("echo may expand only the exit status ($?)")?;
                    if value.starts_with('-') || value.contains('\\') {
                        return Err("echo options and escape sequences are unsupported");
                    }
                }
                result.echo = command.argv[1..].to_vec();
            }
        }
        if !remaining.trim().is_empty() || result.argv.is_empty() {
            return Err("unsupported syntax after the governed command");
        }
        Ok(result)
    }

    /// The broker exposes a success/failure status, not raw Git diagnostics.
    /// A head pipeline succeeds like the shell utility even if push failed.
    pub fn render(&self, output: String, success: bool) -> String {
        let mut output = match self.head {
            Some(count) => output.lines().take(count).collect::<Vec<_>>().join("\n"),
            None => output,
        };
        if !self.echo.is_empty() {
            let status = i32::from(!success && self.head.is_none());
            let echo = self
                .echo
                .iter()
                .map(|word| echo_word(word, status).expect("validated echo"))
                .collect::<Vec<_>>()
                .join(" ");
            output.push('\n');
            output.push_str(&echo);
        }
        output
    }
}

fn echo_word(word: &str, status: i32) -> Option<String> {
    // Literal quoting wins: '$?' and escaped dollars remain literal.
    literal(word).or_else(|| literal(&word.replace("$?", &status.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2719: accept the witnessed presentation, without executing shell code.
    #[test]
    fn familiar_wrappers_preserve_output_and_status() {
        let push =
            GovernedCommand::parse("git push origin HEAD:task 2>&1 | head -5; echo \"exit=$?\"")
                .unwrap();
        assert_eq!(push.argv, ["git", "push", "origin", "HEAD:task"]);
        assert_eq!(
            push.render("failed(git_error)".into(), false),
            "failed(git_error)\nexit=0"
        );
        let push = GovernedCommand::parse("git push; echo \"exit=$?\" '$?'").unwrap();
        assert_eq!(
            push.render("failed(git_error)".into(), false),
            "failed(git_error)\nexit=1 $?"
        );
    }

    /// #2719: the flattened inventory must not erase shell control flow or IO.
    #[test]
    fn unsafe_or_unimplemented_wrappers_are_not_silently_discarded() {
        for source in [
            "git push &",
            "git push || echo nope",
            "git push && head -5",
            "git push | head -5 &",
            "git push > file",
            "2>&1 git push",
            "X=1 git push",
            "git push; X=1 echo ok",
            "git push; touch marker",
            "git push | head file",
            "git push | head -0",
            "git push; echo $(touch marker)",
            "git push; echo $SECRET",
            "git push; echo -e hi",
            "(git push)",
            "git push; echo ok; git push",
            "git push | head -5 > file",
            "git push; echo ${x:=bad}",
            "git push; echo `id`",
        ] {
            assert!(GovernedCommand::parse(source).is_err(), "{source}");
        }
    }
}
