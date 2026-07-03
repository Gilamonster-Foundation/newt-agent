//! A tiny line-command REPL: one `Session`, slash commands for control,
//! anything else appended to the running conversation.

/// What the dispatcher tells the read-loop to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Keep reading input.
    Continue,
    /// Stop the program.
    Exit,
}

/// One interactive session: the live conversation plus close bookkeeping.
#[derive(Default)]
pub struct Session {
    /// Lines of the conversation currently in progress.
    pub conversation: Vec<String>,
    /// How many conversations have been closed out so far.
    pub closed_count: usize,
    /// The reason recorded for the most recent close ("new", "end", …).
    pub last_reason: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once the current conversation has reached its end (no lines
    /// pending). Unrelated to the `/end` command — this is about input.
    pub fn at_end_of_input(&self) -> bool {
        self.conversation.is_empty()
    }

    /// Close out the current conversation: bump the counter, record the
    /// reason, and clear the live lines. Shared by every closing command.
    fn close_conversation(&mut self, reason: &str) {
        self.closed_count += 1;
        self.last_reason = Some(reason.to_string());
        self.conversation.clear();
    }

    /// Dispatch one input line. Slash commands are control; anything else
    /// is conversation.
    pub fn dispatch(&mut self, line: &str) -> Outcome {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('/') {
            // `/new` · `/end` · `/restart` all close out the current
            // conversation and start a fresh one (the three are aliases).
            // The only difference is the reason recorded.
            let close_word = match name {
                "new" => Some("new"),
                "end" => Some("end"),
                "restart" => Some("restart"),
                _ => None,
            };
            if let Some(reason) = close_word {
                self.close_conversation(reason);
                return Outcome::Continue;
            }
            if name == "quit" || name == "exit" {
                return Outcome::Exit;
            }
            // Unknown slash command: ignored, loop continues.
            return Outcome::Continue;
        }
        self.conversation.push(trimmed.to_string());
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_join_the_conversation() {
        let mut s = Session::new();
        assert_eq!(s.dispatch("hello"), Outcome::Continue);
        assert_eq!(s.conversation, vec!["hello"]);
        assert!(!s.at_end_of_input());
    }

    #[test]
    fn new_closes_out_and_continues() {
        let mut s = Session::new();
        s.dispatch("hello");
        assert_eq!(s.dispatch("/new"), Outcome::Continue);
        assert_eq!(s.closed_count, 1);
        assert_eq!(s.last_reason.as_deref(), Some("new"));
        assert!(s.at_end_of_input());
    }

    #[test]
    fn restart_records_its_own_reason() {
        let mut s = Session::new();
        assert_eq!(s.dispatch("/restart"), Outcome::Continue);
        assert_eq!(s.last_reason.as_deref(), Some("restart"));
    }

    #[test]
    fn quit_exits_without_closing() {
        let mut s = Session::new();
        s.dispatch("hello");
        assert_eq!(s.dispatch("/quit"), Outcome::Exit);
        assert_eq!(s.closed_count, 0, "/quit abandons; it does not close out");
    }
}
