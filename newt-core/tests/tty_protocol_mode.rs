//! §6.3 — **MCP/worker stdio purity on EVERY platform**, not by fd-numbering
//! luck.
//!
//! `newt-cli/tests/stdout_purity.rs` is `#![cfg(unix)]`, because the protection
//! it tests is a unix-only `dup2(2, 1)` in `stdio_guard.rs` (whose non-unix arm
//! returns `ErrorKind::Unsupported`). On Windows fd 1 IS the live JSON-RPC
//! channel and nothing guarded it — a spinner frame could interleave into a
//! protocol frame and no test could see it.
//!
//! `tty::enter_protocol_mode()` is the platform-independent guard, and this is
//! its test. It runs everywhere.
//!
//! Lives in its own integration binary (= its own PROCESS) on purpose:
//! protocol mode is deliberately **irreversible**, so exercising it inside the
//! unit-test binary would silently disable the line for every other test that
//! shares that process.

use newt_core::tty::{self, LineCaps, Sink, Spinner, Terminal};

#[test]
fn protocol_mode_vetoes_the_line_absolutely_and_irreversibly() {
    // Before: whatever this environment allows — but the OVERRIDE seam (the
    // step-4 compatibility shim that lets a legacy caller force `Own`) can
    // certainly obtain a lease.
    let pre = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout);
    assert!(
        pre.is_some(),
        "precondition: an explicit Own override yields the line before protocol mode"
    );
    drop(pre);

    tty::enter_protocol_mode();
    assert!(tty::protocol_mode());

    // The detected capability collapses...
    assert_eq!(
        tty::LineCaps::detect(),
        LineCaps::None,
        "a JSON-RPC wire is never an ownable line"
    );
    // ...and, crucially, the OVERRIDE cannot pierce it. This is the property
    // that protects Windows: the migration shim preserves legacy gating for
    // human terminals but can never re-enable painting onto a protocol channel.
    assert!(
        Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).is_none(),
        "protocol mode must veto even an explicit Own override"
    );

    // And the spinner writes ZERO bytes: it does not construct at all.
    assert!(
        Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, true).is_none(),
        "no spinner may exist on a protocol channel"
    );
    assert!(
        Spinner::start("thinking…", Sink::Stdout, true).is_none(),
        "the detected path agrees"
    );

    // Irreversible: a second call changes nothing, and there is no way back.
    tty::enter_protocol_mode();
    assert!(Terminal::lease_with_caps(LineCaps::Own, Sink::Stderr).is_none());
}
