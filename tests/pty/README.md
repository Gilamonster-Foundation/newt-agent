# tests-pty

The shared, Unix-only PTY harness for Newt-Agent's real-terminal tests.
It depends only on `libc`, so low-level crates can use it without a dependency cycle.

`Pty` owns a terminal pair, continuously drains child output, and provides bounded,
non-consuming output waits. Use `termios_snapshot` to compare complete inherited
terminal settings; `is_raw` checks only canonical input and echo. The screen-grid
helper supports limited cursor-position checks, not full terminal emulation.

Run `cargo test -p tests-pty`. Tests ground mocked terminal behavior with real
subprocesses and kernel terminal state. This internal crate is not published.

Licensed under Apache-2.0, like the workspace.
