//! Preserve filesystem outcome classes before presentation erases IO errors.
use crate::ExecOutcome;
use std::{io, sync::OnceLock};

#[derive(Debug)]
pub(super) struct IoFailure {
    output: String,
    outcome: Option<ExecOutcome>,
}

impl IoFailure {
    pub(super) fn denied(output: String) -> Self {
        Self {
            output,
            outcome: Some(ExecOutcome::Denied),
        }
    }

    pub(super) fn io(error: &io::Error, output: String) -> Self {
        use io::ErrorKind;
        let outcome = match error.kind() {
            ErrorKind::PermissionDenied => Some(ExecOutcome::Denied),
            ErrorKind::TimedOut => Some(ExecOutcome::TimedOut),
            ErrorKind::NotFound
            | ErrorKind::AlreadyExists
            | ErrorKind::InvalidInput
            | ErrorKind::InvalidData
            | ErrorKind::NotADirectory
            | ErrorKind::IsADirectory
            | ErrorKind::DirectoryNotEmpty
            | ErrorKind::WriteZero
            | ErrorKind::UnexpectedEof => Some(ExecOutcome::Failed),
            ErrorKind::Unsupported => Some(ExecOutcome::Unavailable),
            _ => None,
        };
        Self { output, outcome }
    }

    pub(super) fn record(self, slot: Option<&OnceLock<ExecOutcome>>) -> String {
        if let (Some(slot), Some(outcome)) = (slot, self.outcome) {
            let _ = slot.set(outcome);
        }
        self.output
    }
}

// A partial listing cannot claim success when an entry failed to read.
pub(super) fn directory_names(
    entries: impl IntoIterator<Item = io::Result<String>>,
) -> Result<Vec<String>, IoFailure> {
    entries
        .into_iter()
        .collect::<io::Result<Vec<_>>>()
        .map_err(|error| IoFailure::io(&error, format!("error reading directory entry: {error}")))
}

#[cfg(test)]
#[path = "io_failure_tests.rs"]
mod tests;
