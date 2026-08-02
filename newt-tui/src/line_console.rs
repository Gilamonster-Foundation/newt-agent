//! Shared plain-IO console abstraction for line-based, non-widget TUI flows.

use std::io::{self, Write};

/// Line-based console I/O for non-widget interactive flows.
pub trait Console {
    /// Print `prompt` (no trailing newline) and read one trimmed line of input.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Emit an informational line.
    fn say(&mut self, line: &str);
}

/// Real console implementation backed by stdin/stdout.
pub struct StdinConsole;

impl Console for StdinConsole {
    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut buf = String::new();
        let n = io::stdin().read_line(&mut buf)?;
        if n == 0 {
            // EOF (e.g. piped empty input): behave like an empty answer so the
            // caller's default kicks in instead of looping forever.
            return Ok(String::new());
        }
        Ok(buf.trim().to_string())
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

/// Shared `[Y/n]` helper.
pub fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_yes;

    #[test]
    fn is_yes_is_strict() {
        assert!(is_yes("", true));
        assert!(!is_yes("", false));
        assert!(is_yes("y", false));
        assert!(is_yes("YES", false));
        assert!(!is_yes("n", true));
        assert!(!is_yes("no", true));
        assert!(!is_yes("garbage", true));
    }
}
