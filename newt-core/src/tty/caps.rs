//! **The ONE gate**: may this process own an ephemeral terminal line?
//!
//! Before this module the workspace answered that question three different
//! ways — `color && thinking_stream_enabled()`, bare `color`, and an upstream
//! `is_terminal()` — and the permission subsystem used a fourth (`interactive`
//! = stdin AND stdout are real terminals). The worst of those is `color`: a
//! *rendering-capability* signal doing the job of an *I/O-ownership* signal, so
//! `NEWT_COLOR=always | tee log.txt` sprayed spinner frames into a captured log.
//!
//! [`LineCaps`] is the single answer. `color` is demoted to styling and is
//! never sufficient to animate.

use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// What this process may do to the terminal's ephemeral bottom line. Distinct
/// from "does the user want ANSI colors" — that stays `ColorMode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LineCaps {
    /// stdin+stdout are real TTYs, `TERM` is usable, no protocol guard active.
    Own,
    /// May emit color but MUST NOT own an ephemeral line (pipe, forced color
    /// into a capture, `TERM=dumb`, the MCP/worker protocol tier).
    None,
}

impl LineCaps {
    /// Can this process paint (and reliably erase) an ephemeral line?
    pub fn can_own(self) -> bool {
        matches!(self, Self::Own)
    }
}

/// Irreversible: this process speaks a machine protocol on fd 1 (JSON-RPC for
/// `newt worker` / `newt mcp serve`). Nothing may ever paint an ephemeral line
/// again.
///
/// Called by `newt-cli` at the worker and MCP entry points on **every
/// platform**. It deliberately does NOT rely on the unix-only `dup2` in
/// `newt-cli/src/stdio_guard.rs`, whose non-unix arm returns
/// `ErrorKind::Unsupported` — on Windows fd 1 IS the live protocol channel and
/// this flag is the only thing standing between a spinner frame and a corrupted
/// JSON-RPC stream.
static PROTOCOL_MODE: AtomicBool = AtomicBool::new(false);

/// Enter protocol mode. Idempotent and one-way — there is no leaving it.
pub fn enter_protocol_mode() {
    PROTOCOL_MODE.store(true, Ordering::SeqCst);
}

/// Is fd 1 a machine protocol channel?
pub fn protocol_mode() -> bool {
    PROTOCOL_MODE.load(Ordering::SeqCst)
}

/// The pure predicate behind [`LineCaps::detect`], split out so the gate is
/// table-testable over the whole cartesian product without a terminal.
///
/// `NO_COLOR` and `NEWT_COLOR` are deliberately **absent**. They decide
/// *styling*, not *ownership*: a real TTY with `NO_COLOR=1` still deserves a
/// liveness indication (rendered plain), and `NEWT_COLOR=always` into a pipe is
/// already `None` because stdout is not a terminal. Folding color policy back
/// into this predicate would re-create exactly the conflation this module
/// exists to remove.
pub(crate) fn probe(
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&str>,
    protocol: bool,
) -> LineCaps {
    // A protocol channel on fd 1 vetoes everything, on every platform.
    if protocol {
        return LineCaps::None;
    }
    // Both halves must be real terminals — the same predicate the permission
    // subsystem calls `interactive`. stdout alone is not enough: an ephemeral
    // line only makes sense for a human who can also answer.
    if !stdin_tty || !stdout_tty {
        return LineCaps::None;
    }
    // `TERM=dumb` (and an absent/empty TERM) means no cursor addressing, so an
    // ephemeral line could be painted but never erased.
    match term {
        None | Some("") | Some("dumb") => LineCaps::None,
        Some(_) => LineCaps::Own,
    }
}

/// This process's line capability.
///
/// The *terminal probe* is memoized (fd-ness and `TERM` do not change under a
/// running process), but [`protocol_mode`] is consulted **live** on every call —
/// `enter_protocol_mode()` may run after something has already asked, and a
/// stale `Own` there is the Windows JSON-RPC corruption bug.
impl LineCaps {
    /// See [`detect`].
    pub fn detect() -> Self {
        detect()
    }
}

/// This process's line capability.
pub fn detect() -> LineCaps {
    static PROBE: OnceLock<LineCaps> = OnceLock::new();
    if protocol_mode() {
        return LineCaps::None;
    }
    *PROBE.get_or_init(|| {
        probe(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            std::env::var("TERM").ok().as_deref(),
            false,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{probe, LineCaps};

    /// §6.7: the gate is false in every non-ownable configuration. Table-driven
    /// over (stdin tty) × (stdout tty) × TERM × protocol_mode.
    #[test]
    fn only_two_real_ttys_with_a_usable_term_may_own_the_line() {
        let cases: &[(bool, bool, Option<&str>, bool, LineCaps)] = &[
            // The one ownable shape.
            (true, true, Some("xterm-256color"), false, LineCaps::Own),
            (true, true, Some("screen"), false, LineCaps::Own),
            // A protocol channel on fd 1 vetoes even a perfect terminal — this
            // is the Windows MCP protection, where the fd-redirect is absent.
            (true, true, Some("xterm-256color"), true, LineCaps::None),
            // Either half piped is disqualifying.
            (false, true, Some("xterm"), false, LineCaps::None),
            (true, false, Some("xterm"), false, LineCaps::None),
            (false, false, Some("xterm"), false, LineCaps::None),
            // No cursor addressing.
            (true, true, Some("dumb"), false, LineCaps::None),
            (true, true, Some(""), false, LineCaps::None),
            (true, true, None, false, LineCaps::None),
        ];
        for &(sin, sout, term, proto, want) in cases {
            assert_eq!(
                probe(sin, sout, term, proto),
                want,
                "stdin_tty={sin} stdout_tty={sout} TERM={term:?} protocol={proto}"
            );
        }
    }

    /// The load-bearing regression: forced color into a PIPE must NOT animate.
    /// Before the arbiter, `NEWT_COLOR=always` set `color = true` and `color`
    /// alone drove the spinner, so frames sprayed into captured logs. `color` is
    /// not an input here at all — piped stdout is `None` and that is the end of
    /// it.
    #[test]
    fn forced_color_into_a_pipe_can_never_own_the_line() {
        assert_eq!(
            probe(true, false, Some("xterm-256color"), false),
            LineCaps::None
        );
    }

    /// The mirror-image regression from the same conflation: a REAL terminal
    /// whose user set `NO_COLOR` still gets liveness (rendered plain). Color
    /// policy is not an ownership input.
    #[test]
    fn no_color_policy_does_not_revoke_line_ownership() {
        assert_eq!(
            probe(true, true, Some("xterm-256color"), false),
            LineCaps::Own,
            "NO_COLOR must dim the line, not delete it"
        );
    }
}
