//! Config-read diagnostics belong to the CLI, including failed reads.

use newt_core::tty::{LineCaps, Notice};

pub(crate) fn read<T>(operation: impl FnOnce(&mut dyn FnMut(Notice<'static>)) -> T) -> T {
    read_with(operation, |notice| {
        // The CLI owns plain stderr here, including protocol and pipe modes.
        let _ = notice.diagnostic(LineCaps::None, false, std::io::stderr());
    })
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

#[cfg(test)]
#[path = "migration_notices_tests.rs"]
mod tests;
