//! Migration notices at the TUI's existing presentation boundary.

use std::io::IsTerminal as _;

use newt_core::tty::{LineCaps, Notice};

pub(crate) fn emit(notice: Notice<'static>) {
    let caps = if std::io::stderr().is_terminal() {
        LineCaps::detect()
    } else {
        LineCaps::None
    };
    // A cockpit-owned stderr feeds its existing PTY capture. Redirected stderr
    // remains the operator's file/pipe. Plain text also avoids a config read
    // merely to decide how to style the diagnostic of that same config read.
    let _ = notice.diagnostic(caps, false, std::io::stderr());
}

pub(crate) fn read<T>(operation: impl FnOnce(&mut dyn FnMut(Notice<'static>)) -> T) -> T {
    read_with(operation, emit)
}

fn read_with<T>(
    operation: impl FnOnce(&mut dyn FnMut(Notice<'static>)) -> T,
    mut deliver: impl FnMut(Notice<'static>),
) -> T {
    let mut notices = Vec::new();
    let result = operation(&mut |notice| notices.push(notice));
    for notice in notices {
        deliver(notice);
    }
    result
}

/// Startup holds this outside its splash guard, so even an error unwinds the
/// screen before diagnostics are flushed. Explicit flush releases values before
/// the session starts; Drop covers the early-return paths.
#[derive(Default)]
pub(crate) struct Pending(Vec<Notice<'static>>);

impl Pending {
    pub(crate) fn report(&mut self, notice: Notice<'static>) {
        self.0.push(notice);
    }

    pub(crate) fn flush(&mut self) {
        for notice in self.0.drain(..) {
            emit(notice);
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
#[path = "migration_notices_tests.rs"]
mod tests;
